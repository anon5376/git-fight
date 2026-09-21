//! GitHub user authorization. Token is dropped after GET /user.

use crate::db;
use axum::extract::{Query, State};
use axum::http::header::{HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use sqlx::SqlitePool;

const COOKIE: &str = "git_fight_sid";
const STATE_COOKIE: &str = "git_fight_oauth";

#[derive(Clone)]
pub struct Auth {
    pub session_key: Vec<u8>,
    pub public_url: String,
}

impl Default for Auth {
    fn default() -> Self {
        Self {
            session_key: vec![0x11; 32],
            public_url: "http://127.0.0.1:8080".into(),
        }
    }
}

pub fn parse_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let raw = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(name).and_then(|s| s.strip_prefix('=')) {
            return Some(v);
        }
    }
    None
}

pub fn sign(key: &[u8], value: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)
        .unwrap_or_else(|_| Hmac::<Sha256>::new_from_slice(&[0u8; 32]).expect("hmac"));
    mac.update(value.as_bytes());
    format!("{value}.{}", hex::encode(mac.finalize().into_bytes()))
}

pub fn verify_signed(key: &[u8], cookie: &str) -> Option<String> {
    let (value, sig_hex) = cookie.rsplit_once('.')?;
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(value.as_bytes());
    let expect = mac.finalize().into_bytes();
    let mut claimed = [0u8; 32];
    if sig_hex.len() != 64 || hex::decode_to_slice(sig_hex.as_bytes(), &mut claimed).is_err() {
        return None;
    }
    use subtle::ConstantTimeEq;
    if bool::from(expect.as_slice().ct_eq(&claimed)) {
        Some(value.to_string())
    } else {
        None
    }
}

pub async fn login_from_headers(
    pool: &SqlitePool,
    key: &[u8],
    headers: &HeaderMap,
) -> Option<String> {
    let raw = parse_cookie(headers, COOKIE)?;
    let id = verify_signed(key, raw)?;
    db::session_login(pool, &id).await.ok().flatten()
}

#[derive(Deserialize)]
pub struct AuthQuery {
    #[serde(default)]
    r#return: Option<String>,
}

pub async fn start_auth(
    State(state): State<crate::app::AppState>,
    Query(q): Query<AuthQuery>,
) -> Response {
    let Some(gh) = &state.github else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "github login is not configured",
        )
            .into_response();
    };
    let redirect = format!(
        "{}/auth/github/callback",
        state.auth.public_url.trim_end_matches('/')
    );
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let ret = sanitize_return(q.r#return.as_deref());
    let state_val = format!("{nonce}:{ret}");
    let signed = sign(&state.auth.session_key, &state_val);
    let url = gh.authorize_url(&redirect, &nonce);
    let cookie = format!("{STATE_COOKIE}={signed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=600");
    let mut headers = HeaderMap::new();
    headers.insert(
        LOCATION,
        HeaderValue::from_str(&url).unwrap_or(HeaderValue::from_static("/")),
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        headers.insert(SET_COOKIE, v);
    }
    (StatusCode::FOUND, headers).into_response()
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

pub async fn auth_callback(
    State(state): State<crate::app::AppState>,
    Query(q): Query<CallbackQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(gh) = &state.github else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "github login is not configured",
        )
            .into_response();
    };
    let Some(code) = q.code else {
        return (StatusCode::BAD_REQUEST, "missing code").into_response();
    };
    let stored = parse_cookie(&headers, STATE_COOKIE)
        .and_then(|c| verify_signed(&state.auth.session_key, c));
    let Some(stored) = stored else {
        return (StatusCode::BAD_REQUEST, "bad state").into_response();
    };
    let Some((nonce, ret)) = stored.split_once(':') else {
        return (StatusCode::BAD_REQUEST, "bad state").into_response();
    };
    if q.state.as_deref() != Some(nonce) {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }
    let redirect = format!(
        "{}/auth/github/callback",
        state.auth.public_url.trim_end_matches('/')
    );
    let (user_id, login) = match gh.oauth_user(&code, &redirect).await {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_GATEWAY, "oauth failed").into_response(),
    };
    let sid = uuid::Uuid::new_v4().simple().to_string();
    if db::insert_session(&state.pool, &sid, user_id, &login)
        .await
        .is_err()
    {
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }
    let signed = sign(&state.auth.session_key, &sid);
    let cookie = format!("{COOKIE}={signed}; Path=/; HttpOnly; SameSite=Lax; Max-Age=1209600");
    let dest = sanitize_return(Some(ret));
    let mut out = HeaderMap::new();
    out.insert(
        LOCATION,
        HeaderValue::from_str(&dest).unwrap_or(HeaderValue::from_static("/")),
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        out.insert(SET_COOKIE, v);
    }
    (StatusCode::FOUND, out).into_response()
}

fn sanitize_return(r: Option<&str>) -> String {
    match r {
        Some(s) if s.starts_with('/') && !s.starts_with("//") && !s.contains('\n') => s.to_string(),
        _ => "/".into(),
    }
}

pub async fn me(State(state): State<crate::app::AppState>, headers: HeaderMap) -> Response {
    match login_from_headers(&state.pool, &state.auth.session_key, &headers).await {
        Some(login) => axum::Json(serde_json::json!({ "login": login })).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}
