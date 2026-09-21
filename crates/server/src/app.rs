use crate::auth::{self, Auth};
use crate::db::{self, MatchRow};
use crate::gh::GitHub;
use crate::protocol::{ClientMsg, Role, DISCONNECT_SECS, EXPIRE_SECS, INPUT_DELAY};
use crate::result::ResultCtx;
use crate::room::{self, RoomEvent, RoomSettings};
use crate::webhook;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, StatusCode};
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
    pub github: Option<GitHub>,
    pub auth: Auth,
    pub webhook_secret: Option<Vec<u8>>,
    pub test_repos: HashMap<String, PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            lag: Duration::ZERO,
            instant: false,
            static_dir: None,
            expire_secs: EXPIRE_SECS,
            disconnect: Duration::from_secs(DISCONNECT_SECS),
            github: None,
            auth: Auth::default(),
            webhook_secret: None,
            test_repos: HashMap::new(),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Config,
    rooms: Arc<Mutex<HashMap<String, mpsc::Sender<RoomEvent>>>>,
    pub github: Option<GitHub>,
    pub auth: Auth,
    pub webhook_secret: Option<Vec<u8>>,
    pub test_repos: HashMap<String, PathBuf>,
}

pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/health", get(health))
        .route("/api/matches", post(create_match))
        .route("/api/matches/{id}", get(get_match))
        .route("/api/replays/{id}", get(get_replay))
        .route("/ws", get(ws_upgrade))
        .route("/webhooks/github", post(webhook::github_webhook))
        .route("/auth/github", get(auth::start_auth))
        .route("/auth/github/callback", get(auth::auth_callback))
        .route("/api/me", get(auth::me))
        .route("/match/{id}", get(spa))
        .route("/replay/{id}", get(spa))
        .route("/{owner}/{repo}/leaderboard", get(leaderboard))
        .route("/badge/{owner}/{repo}/{user}", get(badge))
        .with_state(state.clone());

    if let Some(dir) = &state.config.static_dir {
        let index = dir.join("index.html");
        app = app.fallback_service(ServeDir::new(dir).not_found_service(ServeFile::new(index)));
    }
    app
}

impl AppState {
    async fn room_tx(&self, row: &MatchRow) -> mpsc::Sender<RoomEvent> {
        let mut rooms = self.rooms.lock().await;
        rooms
            .entry(row.id.clone())
            .or_insert_with(|| {
                room::spawn_room(
                    row.clone(),
                    self.pool.clone(),
                    RoomSettings {
                        instant: self.config.instant,
                        disconnect: self.config.disconnect,
                        result: Some(ResultCtx {
                            gh: self.github.clone(),
                            pool: self.pool.clone(),
                            public_url: self.auth.public_url.clone(),
                            test_repos: self.test_repos.clone(),
                        }),
                    },
                )
            })
            .clone()
    }
}

pub async fn serve(listener: TcpListener, pool: SqlitePool, config: Config) -> std::io::Result<()> {
    let state = AppState {
        pool: pool.clone(),
        github: config.github.clone(),
        auth: config.auth.clone(),
        webhook_secret: config.webhook_secret.clone(),
        test_repos: config.test_repos.clone(),
        config,
        rooms: Arc::new(Mutex::new(HashMap::new())),
    };
    if let Ok(rows) = db::list_live_matches(&state.pool).await {
        for row in rows {
            let _ = state.room_tx(&row).await;
        }
    }
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

async fn leaderboard(
    State(state): State<AppState>,
    Path((owner, repo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    crate::stats::leaderboard_response(&state.pool, &owner, &repo, &headers).await
}

async fn badge(
    State(state): State<AppState>,
    Path((owner, repo, user)): Path<(String, String, String)>,
) -> Response {
    crate::stats::badge_response(&state.pool, &owner, &repo, &user).await
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
    ours_login: Option<String>,
    theirs_login: Option<String>,
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
        ours_login: row.ours_login,
        theirs_login: row.theirs_login,
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
    ours_hp: i32,
    ours_armor: bool,
    ours_special: bool,
    theirs_hp: i32,
    theirs_armor: bool,
    theirs_special: bool,
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
    let hunks = db::list_hunks(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let (ours_stats, theirs_stats) = hunks
        .last()
        .map(|h| (h.ours_stats(), h.theirs_stats()))
        .unwrap_or_default();
    Ok(Json(ReplayOut {
        id: row.id,
        seed: row.seed,
        ticks: inputs.into_iter().map(|(_, o, t)| [o, t]).collect(),
        final_hash: row.final_hash,
        status: row.status,
        ours_hp: ours_stats.hp,
        ours_armor: ours_stats.armor,
        ours_special: ours_stats.special,
        theirs_hp: theirs_stats.hp,
        theirs_armor: theirs_stats.armor,
        theirs_special: theirs_stats.special,
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
    headers: HeaderMap,
) -> Response {
    let login = auth::login_from_headers(&state.pool, &state.auth.session_key, &headers).await;
    ws.on_upgrade(move |socket| handle_socket(socket, state, q, login))
}

async fn handle_socket(socket: WebSocket, state: AppState, q: WsQuery, login: Option<String>) {
    let Ok(Some(row)) = db::get_match(&state.pool, &q.match_id).await else {
        return;
    };
    if row.status == "expired" {
        return;
    }
    let role = role_for(&row, q.token.as_deref(), login.as_deref());
    let tx = state.room_tx(&row).await;

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
            let Ok(ClientMsg::Input {
                tick,
                buttons,
                theirs,
            }) = serde_json::from_str::<ClientMsg>(&text)
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
                    theirs_buttons: theirs,
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

fn role_for(row: &MatchRow, token: Option<&str>, login: Option<&str>) -> Role {
    if row.ours_login.is_some() || row.theirs_login.is_some() {
        let Some(login) = login else {
            return Role::Spectator;
        };
        let ours = row.ours_login.as_deref() == Some(login);
        let theirs = row.theirs_login.as_deref() == Some(login);
        return match (ours, theirs) {
            (true, true) => Role::Both,
            (true, false) => Role::Ours,
            (false, true) => Role::Theirs,
            (false, false) => Role::Spectator,
        };
    }
    match token {
        Some(t) if row.ours_token.as_deref() == Some(t) => Role::Ours,
        Some(t) if row.theirs_token.as_deref() == Some(t) => Role::Theirs,
        _ => Role::Spectator,
    }
}
