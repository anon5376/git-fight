use crate::auth::{self, Auth};
use crate::db::{self, MatchRow};
use crate::gh::GitHub;
use crate::protocol::{
    can_enqueue_input, closed_ws_message, is_match_id, round_seed, split_seed, ClientMsg,
    ServerMsg, DISCONNECT_SECS, EXPIRE_SECS, INPUT_DELAY,
};
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc::error::TrySendError;
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

impl Config {
    /// A GitHub-backed process must have a webhook HMAC secret, a
    /// non-default session key, and a public URL that is not a wildcard bind.
    pub fn require_live_github_secrets(&self) -> Result<(), &'static str> {
        if self.github.is_none() {
            return Ok(());
        }
        if self.webhook_secret.as_ref().map(|s| s.len()).unwrap_or(0) < 8 {
            return Err("GITHUB_WEBHOOK_SECRET");
        }
        if self.auth.session_key.len() < 16 || self.auth.session_key.iter().all(|&b| b == 0x11) {
            return Err("SESSION_KEY");
        }
        if !is_live_public_url(&self.auth.public_url) {
            return Err("GIT_FIGHT_PUBLIC_URL");
        }
        if let Some(gh) = &self.github {
            if !gh.endpoints_are_github() {
                return Err("GITHUB_API_URL");
            }
        }
        Ok(())
    }
}

/// Match links and OAuth redirect_uri. Reject wildcard binds (`0.0.0.0`).
fn is_live_public_url(s: &str) -> bool {
    let s = s.trim();
    if !(12..=200).contains(&s.len()) {
        return false;
    }
    let rest = if let Some(r) = s.strip_prefix("https://") {
        r
    } else if let Some(r) = s.strip_prefix("http://") {
        r
    } else {
        return false;
    };
    if rest.is_empty()
        || rest.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '\\' | '?' | '#' | '@'))
    {
        return false;
    }
    let hostport = rest.split('/').next().unwrap_or("");
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    !host.is_empty() && host != "0.0.0.0" && host != "*" && host != "::" && !host.starts_with('-')
}

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub config: Config,
    rooms: Arc<Mutex<HashMap<String, mpsc::Sender<RoomEvent>>>>,
    publishing: Arc<Mutex<HashSet<String>>>,
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
    fn result_ctx(&self) -> ResultCtx {
        ResultCtx {
            gh: self.github.clone(),
            pool: self.pool.clone(),
            public_url: self.auth.public_url.clone(),
            test_repos: self.test_repos.clone(),
            publishing: self.publishing.clone(),
        }
    }

    async fn record_missing_stats(&self) {
        let Ok(ids) = db::list_unrecorded_stat_matches(&self.pool).await else {
            return;
        };
        for id in ids {
            let hunks = db::list_hunks(&self.pool, &id).await.unwrap_or_default();
            crate::stats::record_stored_winners(&self.pool, &id, &hunks).await;
        }
    }

    async fn finish_scored_open(&self) {
        self.record_missing_stats().await;
        let Ok(ids) = db::list_scored_open_matches(&self.pool).await else {
            return;
        };
        for id in ids {
            let Some(row) = db::get_match(&self.pool, &id).await.ok().flatten() else {
                continue;
            };
            let hunks = db::list_hunks(&self.pool, &id).await.unwrap_or_default();
            if hunks.is_empty() || hunks.iter().any(|h| h.winner.is_none()) {
                continue;
            }
            crate::stats::record_stored_winners(&self.pool, &id, &hunks).await;
            let seed: u64 = row.seed.parse().unwrap_or(1);
            let last = u32::try_from(hunks.len().saturating_sub(1)).unwrap_or(0);
            let Some(hash) =
                room::hash_from_stored_round(&self.pool, &id, seed, &hunks, last).await
            else {
                continue;
            };
            if db::finish_open_match(&self.pool, &id, &hash)
                .await
                .unwrap_or(false)
            {
                self.result_ctx().spawn_publish(id.clone());
                self.close_room(&id).await;
            }
        }
    }

    fn retry_unpublished(&self) {
        let ctx = self.result_ctx();
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let Ok(ids) = db::list_unpublished_results(&pool).await else {
                return;
            };
            for id in ids {
                ctx.spawn_publish(id);
            }
        });
    }

    async fn room_tx(&self, row: &MatchRow) -> Result<mpsc::Sender<RoomEvent>, String> {
        let Some(fresh) = db::get_match(&self.pool, &row.id).await.ok().flatten() else {
            return Err("not found".into());
        };
        if let Some(message) = closed_ws_message(&fresh.status, fresh.abort_reason.as_deref()) {
            return Err(message.to_string());
        }
        let mut rooms = self.rooms.lock().await;
        if let Some(existing) = rooms.get(&fresh.id) {
            if !existing.is_closed() {
                return Ok(existing.clone());
            }
            rooms.remove(&fresh.id);
        }
        let tx = room::spawn_room(
            fresh.clone(),
            self.pool.clone(),
            RoomSettings {
                instant: self.config.instant,
                disconnect: self.config.disconnect,
                result: Some(self.result_ctx()),
            },
            self.rooms.clone(),
        );
        rooms.insert(fresh.id, tx.clone());
        Ok(tx)
    }

    pub(crate) async fn close_room(&self, id: &str) {
        let tx = {
            let mut rooms = self.rooms.lock().await;
            rooms.remove(id)
        };
        if let Some(tx) = tx {
            if tx.try_send(RoomEvent::Shutdown).is_err() {
                tokio::spawn(async move {
                    let _ = tx.send(RoomEvent::Shutdown).await;
                });
            }
        }
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
        publishing: Arc::new(Mutex::new(HashSet::new())),
    };
    if let Ok(rows) = db::list_live_matches(&state.pool).await {
        for row in rows {
            let _ = state.room_tx(&row).await;
        }
    }
    state.finish_scored_open().await;
    state.retry_unpublished();
    let expirer = state.clone();
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_secs(5));
        loop {
            iv.tick().await;
            expirer.finish_scored_open().await;
            if let Ok(ids) = db::expire_pending(&expirer.pool).await {
                for id in ids {
                    expirer.close_room(&id).await;
                    let expirer = expirer.clone();
                    tokio::spawn(async move {
                        if let Ok(Some(row)) = db::get_match(&expirer.pool, &id).await {
                            if row.status == "expired" {
                                crate::result::comment_expired(&expirer.result_ctx(), &row).await;
                            }
                        }
                    });
                }
            }
            expirer.retry_unpublished();
            let _ = db::prune_deliveries(&expirer.pool, crate::limits::WEBHOOK_MAX_AGE_SECS).await;
            let _ = db::prune_sessions(&expirer.pool).await;
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
    // Live GitHub App fights start from `/fight`. Anonymous host is local-only.
    if state.github.is_some() {
        return Err(StatusCode::NOT_FOUND);
    }
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
    if !is_match_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
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
struct ReplayRound {
    round: u32,
    seed: String,
    seed_lo: u32,
    seed_hi: u32,
    ticks: Vec<[u8; 2]>,
    path: String,
    hunk_index: u32,
    ours_hp: i32,
    ours_armor: bool,
    ours_special: bool,
    theirs_hp: i32,
    theirs_armor: bool,
    theirs_special: bool,
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
    total_rounds: u32,
    rounds: Vec<ReplayRound>,
}

async fn get_replay(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ReplayOut>, StatusCode> {
    if !is_match_id(&id) {
        return Err(StatusCode::NOT_FOUND);
    }
    let row = db::get_match(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    if row.status != "finished" {
        return Err(StatusCode::NOT_FOUND);
    }
    let inputs = db::load_all_inputs(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let hunks = db::list_hunks(&state.pool, &id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut by_round: BTreeMap<u32, Vec<[u8; 2]>> = BTreeMap::new();
    for (round, _tick, o, t) in inputs {
        by_round.entry(round).or_default().push([o, t]);
    }
    let seed: u64 = row.seed.parse().unwrap_or(1);
    let total_rounds = u32::try_from(hunks.len()).unwrap_or(0).max(1);
    let mut rounds = Vec::new();
    for round in 0..total_rounds {
        let (ours_stats, theirs_stats) = db::stats_for_round(&hunks, round);
        let (path, hunk_index) = db::hunk_meta_for_round(&hunks, round);
        let rs = round_seed(seed, round);
        let (seed_lo, seed_hi) = split_seed(rs);
        rounds.push(ReplayRound {
            round,
            seed: rs.to_string(),
            seed_lo,
            seed_hi,
            ticks: by_round.remove(&round).unwrap_or_default(),
            path,
            hunk_index,
            ours_hp: ours_stats.hp,
            ours_armor: ours_stats.armor,
            ours_special: ours_stats.special,
            theirs_hp: theirs_stats.hp,
            theirs_armor: theirs_stats.armor,
            theirs_special: theirs_stats.special,
        });
    }
    let first = rounds.first();
    Ok(Json(ReplayOut {
        id: row.id,
        seed: row.seed,
        ticks: first.map(|r| r.ticks.clone()).unwrap_or_default(),
        final_hash: row.final_hash,
        status: row.status,
        ours_hp: first.map(|r| r.ours_hp).unwrap_or(100),
        ours_armor: first.map(|r| r.ours_armor).unwrap_or(false),
        ours_special: first.map(|r| r.ours_special).unwrap_or(false),
        theirs_hp: first.map(|r| r.theirs_hp).unwrap_or(100),
        theirs_armor: first.map(|r| r.theirs_armor).unwrap_or(false),
        theirs_special: first.map(|r| r.theirs_special).unwrap_or(false),
        total_rounds,
        rounds,
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
    // Inputs are a few dozen bytes. Default 64 MiB frames are a room DoS.
    const MAX_WS_MESSAGE: usize = 8 * 1024;
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| handle_socket(socket, state, q, login))
}

/// `Leave` is `try_send`; a full queue is spawned so the read task
/// cannot stall the disconnect clock.
fn enqueue_leave(tx: mpsc::Sender<RoomEvent>, conn_id: u64) {
    if tx.try_send(RoomEvent::Leave { conn_id }).is_err() {
        tokio::spawn(async move {
            let _ = tx.send(RoomEvent::Leave { conn_id }).await;
        });
    }
}

/// A busy `current_theirs_login` must not drop the current-round
/// theirs. Retry already happened; fall back to the last good login.
fn coalesce_theirs_login(
    lookup: Result<Option<String>, sqlx::Error>,
    cached: &mut Option<String>,
) -> Option<String> {
    match lookup {
        Ok(v) => {
            *cached = v.clone();
            v
        }
        Err(_) => cached.clone(),
    }
}

async fn reject_socket(mut socket: WebSocket, message: &str) {
    let body = serde_json::to_string(&ServerMsg::Error {
        message: message.to_string(),
    })
    .unwrap_or_else(|_| r#"{"type":"error","message":"error"}"#.into());
    let _ = socket.send(Message::Text(body.into())).await;
}

async fn handle_socket(socket: WebSocket, state: AppState, q: WsQuery, login: Option<String>) {
    if !is_match_id(&q.match_id) {
        reject_socket(socket, "not found").await;
        return;
    }
    let Ok(Some(row)) = db::get_match(&state.pool, &q.match_id).await else {
        reject_socket(socket, "not found").await;
        return;
    };
    if let Some(message) = closed_ws_message(&row.status, row.abort_reason.as_deref()) {
        reject_socket(socket, message).await;
        return;
    }
    let hunks = db::list_hunks(&state.pool, &row.id)
        .await
        .unwrap_or_default();
    if row.pr_number > 0 && hunks.is_empty() {
        reject_socket(socket, "preparing").await;
        return;
    }
    let github = db::github_identity(&row, &hunks);
    let mut current_theirs = db::current_theirs_login(&state.pool, &row.id)
        .await
        .ok()
        .flatten();
    let enqueue_join = can_enqueue_input(
        github,
        row.ours_login.as_deref(),
        current_theirs.as_deref(),
        login.as_deref(),
        q.token.as_deref(),
        row.ours_token.as_deref(),
        row.theirs_token.as_deref(),
    );
    let conn_id = uuid::Uuid::new_v4().as_u128() as u64;
    let (out_tx, mut out_rx) = mpsc::channel::<String>(512);
    let login_for_input = login.clone();
    let token_for_input = q.token.clone();
    let ours_login = row.ours_login.clone();
    let ours_token = row.ours_token.clone();
    let theirs_token = row.theirs_token.clone();
    let match_id = row.id.clone();
    let pool = state.pool.clone();
    let mut join = Some(RoomEvent::Join {
        conn_id,
        login,
        token: q.token,
        tx: out_tx,
    });
    let mut live = None;
    for _ in 0..2 {
        match state.room_tx(&row).await {
            Ok(tx) => {
                let Some(ev) = join.take() else {
                    break;
                };
                // Fighter Join may wait. Spectator Join is try_send so a
                // connect flood cannot fill the 512-slot room queue.
                if enqueue_join {
                    match tx.send(ev).await {
                        Ok(()) => {
                            live = Some(tx);
                            break;
                        }
                        Err(sent) => {
                            join = Some(sent.0);
                        }
                    }
                } else {
                    match tx.try_send(ev) {
                        Ok(()) => {
                            live = Some(tx);
                            break;
                        }
                        Err(TrySendError::Closed(ev)) => {
                            join = Some(ev);
                        }
                        Err(TrySendError::Full(_)) => {
                            reject_socket(socket, "busy").await;
                            return;
                        }
                    }
                }
            }
            Err(message) => {
                reject_socket(socket, &message).await;
                return;
            }
        }
    }
    let Some(tx) = live else {
        if let Some(message) = db::get_match(&state.pool, &q.match_id)
            .await
            .ok()
            .flatten()
            .as_ref()
            .and_then(|r| closed_ws_message(&r.status, r.abort_reason.as_deref()))
        {
            reject_socket(socket, message).await;
        }
        return;
    };

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
                round,
            }) = serde_json::from_str::<ClientMsg>(&text)
            else {
                continue;
            };
            let current = if github {
                let looked = match db::current_theirs_login(&pool, &match_id).await {
                    Ok(v) => Ok(v),
                    Err(_) => db::current_theirs_login(&pool, &match_id).await,
                };
                coalesce_theirs_login(looked, &mut current_theirs)
            } else {
                None
            };
            if !can_enqueue_input(
                github,
                ours_login.as_deref(),
                current.as_deref(),
                login_for_input.as_deref(),
                token_for_input.as_deref(),
                ours_token.as_deref(),
                theirs_token.as_deref(),
            ) {
                continue;
            }
            let _ = tx.try_send(RoomEvent::Input {
                conn_id,
                tick,
                buttons,
                theirs_buttons: theirs,
                round,
            });
        }
        // Fighter Leave must not block on a full room queue or the
        // 30s disconnect clock never starts.
        enqueue_leave(leave_tx, conn_id);
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
        // Close after the room drops the join tx (scored-all preparing)
        // so the canvas gets a close and reconnects. A mute socket would
        // sit on Error { preparing } forever.
        let _ = sink.close().await;
    });

    let _ = tokio::join!(read, write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gh::GitHub;

    fn gh() -> GitHub {
        GitHub::new(
            "https://api.github.com".into(),
            "https://github.com".into(),
            1,
            include_str!("../tests/fixtures/app_key.txt").into(),
            "cid".into(),
            "csec".into(),
        )
    }

    #[test]
    fn local_demo_needs_no_secrets() {
        assert!(Config::default().require_live_github_secrets().is_ok());
    }

    #[test]
    fn github_without_webhook_or_session_is_rejected() {
        let mut cfg = Config {
            github: Some(gh()),
            ..Config::default()
        };
        assert_eq!(
            cfg.require_live_github_secrets(),
            Err("GITHUB_WEBHOOK_SECRET")
        );
        cfg.webhook_secret = Some(b"webhook-secret-for-tests".to_vec());
        assert_eq!(cfg.require_live_github_secrets(), Err("SESSION_KEY"));
        cfg.auth.session_key = b"session-key-session-key-session!".to_vec();
        assert!(cfg.require_live_github_secrets().is_ok());
        cfg.auth.public_url = "http://0.0.0.0:8080".into();
        assert_eq!(
            cfg.require_live_github_secrets(),
            Err("GIT_FIGHT_PUBLIC_URL")
        );
        cfg.auth.public_url = "https://fight.example".into();
        assert!(cfg.require_live_github_secrets().is_ok());
        cfg.github = Some(GitHub::new(
            "https://evil.example".into(),
            "https://github.com".into(),
            1,
            include_str!("../tests/fixtures/app_key.txt").into(),
            "cid".into(),
            "csec".into(),
        ));
        assert_eq!(cfg.require_live_github_secrets(), Err("GITHUB_API_URL"));
        cfg.github = Some(GitHub::new(
            "http://evil.example".into(),
            "https://github.com".into(),
            1,
            include_str!("../tests/fixtures/app_key.txt").into(),
            "cid".into(),
            "csec".into(),
        ));
        assert_eq!(cfg.require_live_github_secrets(), Err("GITHUB_API_URL"));
    }

    #[test]
    fn public_url_rejects_wildcard_and_junk() {
        assert!(is_live_public_url("http://127.0.0.1:8080"));
        assert!(is_live_public_url("https://git-fight.example/"));
        assert!(!is_live_public_url("http://0.0.0.0:8080"));
        assert!(!is_live_public_url("http://[::]:8080"));
        assert!(!is_live_public_url("http://evil@fight.example"));
        assert!(!is_live_public_url("ftp://fight.example"));
        assert!(!is_live_public_url("https://evil\n.example"));
        assert!(!is_live_public_url(""));
    }

    #[test]
    fn coalesce_theirs_login_keeps_cache_on_error() {
        let mut cached = Some("bob".into());
        assert_eq!(
            coalesce_theirs_login(Err(sqlx::Error::Protocol("busy".into())), &mut cached)
                .as_deref(),
            Some("bob"),
            "a busy current-round read must not drop theirs Input"
        );
        assert_eq!(cached.as_deref(), Some("bob"));
        assert_eq!(
            coalesce_theirs_login(Ok(Some("carol".into())), &mut cached).as_deref(),
            Some("carol")
        );
        assert_eq!(cached.as_deref(), Some("carol"));
        assert_eq!(
            coalesce_theirs_login(Ok(None), &mut cached).as_deref(),
            None,
            "every hunk scored: no current theirs"
        );
        assert!(cached.is_none());
    }

    #[tokio::test]
    async fn fighter_leave_does_not_wait_on_a_full_event_channel() {
        let (tx, _rx) = mpsc::channel::<RoomEvent>(1);
        tx.try_send(RoomEvent::Input {
            conn_id: 1,
            tick: 0,
            buttons: 0,
            theirs_buttons: None,
            round: Some(0),
        })
        .unwrap();
        tokio::time::timeout(Duration::from_millis(200), async {
            enqueue_leave(tx, 2);
        })
        .await
        .expect("fighter Leave waited on a full room channel");
    }

    #[tokio::test]
    async fn close_room_does_not_wait_on_a_full_event_channel() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        let (tx, _rx) = mpsc::channel::<RoomEvent>(1);
        tx.try_send(RoomEvent::Leave { conn_id: 1 }).unwrap();
        let mut map = HashMap::new();
        map.insert("m1".into(), tx);
        let state = AppState {
            pool,
            config: Config::default(),
            rooms: Arc::new(Mutex::new(map)),
            publishing: Arc::new(Mutex::new(HashSet::new())),
            github: None,
            auth: Auth::default(),
            webhook_secret: None,
            test_repos: HashMap::new(),
        };
        tokio::time::timeout(Duration::from_millis(200), state.close_room("m1"))
            .await
            .expect("close_room waited on a full room channel");
    }
}
