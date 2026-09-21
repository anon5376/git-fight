//! GitHub user authorization. Token is dropped after GET /user.

use crate::db;
use crate::limits::SESSION_TTL_SECS;
use axum::extract::{Query, State};
use axum::http::header::{HeaderMap, HeaderValue, LOCATION, SET_COOKIE};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};
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
    let verifier = pkce_verifier();
    let challenge = pkce_challenge(&verifier);
    let ret = sanitize_return(q.r#return.as_deref());
    let state_val = format!("{nonce}:{verifier}:{ret}");
    let signed = sign(&state.auth.session_key, &state_val);
    let url = gh.authorize_url(&redirect, &nonce, &challenge);
    let cookie = format!(
        "{STATE_COOKIE}={signed}; {}",
        cookie_attrs(&state.auth.public_url, 600)
    );
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
    if !crate::gh::is_safe_oauth_code(&code) {
        return (StatusCode::BAD_REQUEST, "bad code").into_response();
    }
    let stored = parse_cookie(&headers, STATE_COOKIE)
        .and_then(|c| verify_signed(&state.auth.session_key, c));
    let Some(stored) = stored else {
        return (StatusCode::BAD_REQUEST, "bad state").into_response();
    };
    let mut parts = stored.splitn(3, ':');
    let (Some(nonce), Some(verifier), Some(ret)) = (parts.next(), parts.next(), parts.next())
    else {
        return (StatusCode::BAD_REQUEST, "bad state").into_response();
    };
    if q.state.as_deref() != Some(nonce) {
        return (StatusCode::BAD_REQUEST, "state mismatch").into_response();
    }
    let redirect = format!(
        "{}/auth/github/callback",
        state.auth.public_url.trim_end_matches('/')
    );
    let (user_id, login) = match gh.oauth_user(&code, &redirect, verifier).await {
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
    let attrs = cookie_attrs(&state.auth.public_url, SESSION_TTL_SECS);
    let cookie = format!("{COOKIE}={signed}; {attrs}");
    let clear = format!("{STATE_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0");
    let dest = sanitize_return(Some(ret));
    let mut out = HeaderMap::new();
    out.insert(
        LOCATION,
        HeaderValue::from_str(&dest).unwrap_or(HeaderValue::from_static("/")),
    );
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        out.append(SET_COOKIE, v);
    }
    if let Ok(v) = HeaderValue::from_str(&clear) {
        out.append(SET_COOKIE, v);
    }
    (StatusCode::FOUND, out).into_response()
}

fn sanitize_return(r: Option<&str>) -> String {
    let Some(s) = r else {
        return "/".into();
    };
    if !s.starts_with('/')
        || s.starts_with("//")
        || s.contains("..")
        || s.len() > 256
        || s.bytes().any(|b| {
            !matches!(
                b,
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.'
            )
        })
    {
        return "/".into();
    }
    s.to_string()
}

fn cookie_attrs(public_url: &str, max_age: i64) -> String {
    let mut s = format!("Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}");
    if public_url.starts_with("https://") {
        s.push_str("; Secure");
    }
    s
}

fn pkce_verifier() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::URL_SAFE_NO_PAD, digest)
}

pub async fn me(State(state): State<crate::app::AppState>, headers: HeaderMap) -> Response {
    match login_from_headers(&state.pool, &state.auth.session_key, &headers).await {
        Some(login) => axum::Json(serde_json::json!({ "login": login })).into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_s256_matches_rfc7636() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_challenge(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn https_cookies_are_secure() {
        let https = cookie_attrs("https://fight.example", 60);
        assert!(https.contains("Secure"), "{https}");
        assert!(https.contains("HttpOnly"), "{https}");
        assert!(https.contains("SameSite=Lax"), "{https}");
        let http = cookie_attrs("http://127.0.0.1:8080", 60);
        assert!(!http.contains("Secure"), "{http}");
    }

    #[test]
    fn sanitize_return_stays_on_this_host() {
        assert_eq!(sanitize_return(Some("/match/abc")), "/match/abc");
        assert_eq!(
            sanitize_return(Some("/acme/box/leaderboard")),
            "/acme/box/leaderboard"
        );
        assert_eq!(sanitize_return(None), "/");
        assert_eq!(sanitize_return(Some("https://evil.example")), "/");
        assert_eq!(sanitize_return(Some("//evil.example")), "/");
        assert_eq!(sanitize_return(Some("/\\evil.example")), "/");
        assert_eq!(sanitize_return(Some("/; Domain=evil.example")), "/");
        assert_eq!(sanitize_return(Some("/foo\r\nLocation: https://evil")), "/");
        assert_eq!(sanitize_return(Some("/foo\nbar")), "/");
        assert_eq!(sanitize_return(Some("/match/../auth")), "/");
        assert_eq!(sanitize_return(Some(&format!("/{}", "a".repeat(300)))), "/");
    }
}
