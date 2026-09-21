use crate::db::{self, MatchRow};
use crate::protocol::{ClientMsg, Role, DISCONNECT_SECS, EXPIRE_SECS, INPUT_DELAY};
use crate::room::{self, RoomEvent, RoomSettings};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct Config {
    pub lag: Duration,
    pub instant: bool,
    pub static_dir: Option<PathBuf>,
    pub expire_secs: i64,
    pub disconnect: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lag: Duration::ZERO,
            instant: false,
            static_dir: None,
            expire_secs: EXPIRE_SECS,
            disconnect: Duration::from_secs(DISCONNECT_SECS),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Config,
    rooms: Arc<Mutex<HashMap<String, mpsc::Sender<RoomEvent>>>>,
}

pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/matches", post(create_match))
        .route("/api/matches/{id}", get(get_match))
        .route("/api/replays/{id}", get(get_replay))
        .route("/ws", get(ws_upgrade))
        .route("/match/{id}", get(spa))
        .route("/replay/{id}", get(spa))
        .with_state(state.clone());

    if let Some(dir) = &state.config.static_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(ServeDir::new(dir).not_found_service(ServeFile::new(index)));
    }
    app
}

pub async fn serve(listener: TcpListener, pool: SqlitePool, config: Config) -> std::io::Result<()> {
    let state = AppState {
        pool: pool.clone(),
        config,
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };
    let expirer = state.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(5));
        loop {
            iv.tick().await;
            if let Ok(ids) = db::expire_pending(&expirer.pool).await {
                let mut rooms = expirer.rooms.lock().await;
                for id in ids {
                    if let Some(tx) = rooms.remove(&id) {
                        let _ = tx.send(RoomEvent::Shutdown).await;
                    }
                }
            }
        }
    });
    let app = router(state);
    axum::serve(listener, app).await
}

#[derive(Deserialize, Default)]
struct CreateBody {
    seed: Option<u64>,
}

#[derive(Serialize)]
struct CreateOut {
    id: String,
    ours_token: String,
    theirs_token: String,
    seed: String,
}

async fn health() -> &'static str {
    "ok"
}

async fn spa(State(state): State<AppState>) -> Response {
    let Some(dir) = &state.config.static_dir else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(dir.join("index.html")).await {
        Ok(bytes) => ([(CONTENT_TYPE, "text/html; charset=utf-8")], bytes).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn create_match(
    State(state): State<AppState>,
    body: Option<Json<CreateBody>>,
) -> Result<Json<CreateOut>, StatusCode> {
    let seed = body
        .and_then(|b| b.seed)
        .unwrap_or_else(|| uuid::Uuid::new_v4().as_u128() as u64);
    let id = uuid::Uuid::new_v4().simple().to_string();
    let ours_token = uuid::Uuid::new_v4().simple().to_string();
    let theirs_token = uuid::Uuid::new_v4().simple().to_string();
    db::insert_match(
        &state.pool,
        &id,
        seed,
        INPUT_DELAY,
        &ours_token,
        &theirs_token,
        state.config.expire_secs,
    )
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(CreateOut {
        id,
        ours_token,
        theirs_token,
        seed: seed.to_string(),
    }))
}

#[derive(Serialize)]
struct MatchPublic {
    id: String,
    status: String,
    seed: String,
    ours: String,
    theirs: String,
    input_delay: i64,
    abort_reason: Option<String>,
}

async fn get_match(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<MatchPublic>, StatusCode> {
    let row = db::get_match(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(MatchPublic {
        id: row.id,
        status: row.status,
        seed: row.seed,
        ours: row.ours_name,
        theirs: row.theirs_name,
        input_delay: row.input_delay_ticks,
        abort_reason: row.abort_reason,
    }))
}

#[derive(Serialize)]
struct ReplayOut {
    id: String,
    seed: String,
    ticks: Vec<[u8; 2]>,
    final_hash: Option<String>,
    status: String,
}

async fn get_replay(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ReplayOut>, StatusCode> {
    let row = db::get_match(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if row.status != "finished" {
        return Err(StatusCode::NOT_FOUND);
    }
    let inputs = db::load_inputs(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(ReplayOut {
        id: row.id,
        seed: row.seed,
        ticks: inputs.into_iter().map(|(_, o, t)| [o, t]).collect(),
        final_hash: row.final_hash,
        status: row.status,
    }))
}

#[derive(Deserialize)]
struct WsQuery {
    #[serde(rename = "match")]
    match_id: String,
    token: Option<String>,
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state, q))
}

async fn handle_socket(socket: WebSocket, state: AppState, q: WsQuery) {
    let Ok(Some(row)) = db::get_match(&state.pool, &q.match_id).await else {
        return;
    };
    if row.status == "expired" {
        return;
    }
    let role = role_for(&row, q.token.as_deref());
    let tx = {
        let mut rooms = state.rooms.lock().await;
        rooms
            .entry(row.id.clone())
            .or_insert_with(|| {
                room::spawn_room(
                    row.clone(),
                    state.pool.clone(),
                    RoomSettings {
                        instant: state.config.instant,
                        disconnect: state.config.disconnect,
                    },
                )
            })
            .clone()
    };

    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let _ = tx.send(RoomEvent::Join { role, tx: out_tx }).await;

    let (mut sink, mut stream) = socket.split();
    let lag = state.config.lag;
    let leave_tx = tx.clone();
    let read = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else {
                continue;
            };
            if lag > Duration::ZERO {
                tokio::time::sleep(lag).await;
            }
            let Ok(ClientMsg::Input { tick, buttons }) = serde_json::from_str::<ClientMsg>(&text)
            else {
                continue;
            };
            if role == Role::Spectator {
                continue;
            }
            let _ = tx
                .send(RoomEvent::Input {
                    role,
                    tick,
                    buttons,
                })
                .await;
        }
        let _ = leave_tx.send(RoomEvent::Leave { role }).await;
    });

    let write = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if lag > Duration::ZERO {
                tokio::time::sleep(lag).await;
            }
            if sink.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });

    let _ = tokio::join!(read, write);
}

fn role_for(row: &MatchRow, token: Option<&str>) -> Role {
    match token {
        Some(t) if row.ours_token.as_deref() == Some(t) => Role::Ours,
        Some(t) if row.theirs_token.as_deref() == Some(t) => Role::Theirs,
        _ => Role::Spectator,
    }
}
