//! GitHub App HTTP + in-memory installation tokens.

use crate::limits::MAX_FIGHT_YML_BYTES;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct GitHub {
    http: reqwest::Client,
    pub api_base: String,
    pub oauth_base: String,
    app_id: u64,
    pem: Arc<String>,
    client_id: String,
    client_secret: String,
    tokens: Arc<Mutex<HashMap<u64, (String, Instant)>>>,
    poll_wait: Duration,
}

/// A 2xx comment POST without a positive id is not success — callers
/// must not treat `0` as “retry POST”.
pub(crate) fn posted_comment_id(id: Option<u64>) -> Result<u64, String> {
    id.filter(|n| *n > 0).ok_or_else(|| "comment id".into())
}

/// Live GitHub often leaves `mergeable` null for seconds while it computes.
const MERGEABLE_POLL_WAIT: Duration = Duration::from_secs(1);
const MERGEABLE_POLL_CAP: Duration = Duration::from_secs(4);
const MERGEABLE_POLL_TRIES: u32 = 8;
/// Contents JSON for a 4 KiB yaml plus GitHub metadata. Bigger is not a config.
const MAX_CONTENTS_JSON: usize = 16 * 1024;
/// List-commits JSON (`per_page=1`, no `files` patches). Bigger is hostile.
const MAX_COMMITS_JSON: usize = 64 * 1024;
/// Pull/repo/comment JSON. PR bodies are capped by GitHub well under this.
const MAX_API_JSON: usize = 1_048_576;
/// Installation token, OAuth token, and `GET /user` JSON.
const MAX_TOKEN_JSON: usize = 16 * 1024;

/// Blame → GitHub login.
/// `None` is “no account” (CPU) and may fall back to email lookup.
/// `Rejected` is an author.login that is not a GitHub name — CPU, no email
/// fallback (`?author=` is any recent commit with that address, not this SHA).
/// `Unavailable` is a transient HTTP/parse miss and must not be stored as CPU.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginLookup {
    Found(String),
    None,
    Rejected,
    Unavailable,
}

impl std::fmt::Debug for GitHub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHub")
            .field("api_base", &self.api_base)
            .field("app_id", &self.app_id)
            .finish_non_exhaustive()
    }
}

impl GitHub {
    pub fn new(
        api_base: String,
        oauth_base: String,
        app_id: u64,
        pem: String,
        client_id: String,
        client_secret: String,
    ) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("http client"),
            api_base: api_base.trim_end_matches('/').to_string(),
            oauth_base: oauth_base.trim_end_matches('/').to_string(),
            app_id,
            pem: Arc::new(pem),
            client_id,
            client_secret,
            tokens: Arc::new(Mutex::new(HashMap::new())),
            poll_wait: MERGEABLE_POLL_WAIT,
        }
    }

    /// Test hook: shorten mergeable backoff. Production keeps the default.
    pub fn with_poll_wait(mut self, poll_wait: Duration) -> Self {
        self.poll_wait = poll_wait;
        self
    }

    /// Live GitHub App HTTP is HTTPS (or loopback HTTP for tests). Never cleartext.
    pub fn endpoints_are_safe(&self) -> bool {
        is_safe_github_endpoint(&self.api_base) && is_safe_github_endpoint(&self.oauth_base)
    }

    /// Production talks only to github.com, not an arbitrary HTTPS host.
    pub fn endpoints_are_github(&self) -> bool {
        self.endpoints_are_safe()
            && is_github_dot_com_api(&self.api_base)
            && is_github_dot_com_oauth(&self.oauth_base)
    }

    fn encoding_key(&self) -> Result<EncodingKey, String> {
        EncodingKey::from_rsa_pem(self.pem.as_bytes()).map_err(|e| e.to_string())
    }

    fn app_jwt(&self) -> Result<String, String> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        #[derive(Serialize)]
        struct Claims {
            iat: i64,
            exp: i64,
            iss: String,
        }
        let claims = Claims {
            iat: now - 60,
            exp: now + 9 * 60,
            iss: self.app_id.to_string(),
        };
        let mut header = Header::new(Algorithm::RS256);
        header.typ = Some("JWT".into());
        encode(&header, &claims, &self.encoding_key()?).map_err(|e| e.to_string())
    }

    pub async fn installation_token(&self, installation_id: u64) -> Result<String, String> {
        {
            let mut cache = self.tokens.lock().await;
            drop_expired_tokens(&mut cache, Instant::now());
            if let Some((tok, _)) = cache.get(&installation_id) {
                return Ok(tok.clone());
            }
        }
        let jwt = self.app_jwt()?;
        let url = format!(
            "{}/app/installations/{installation_id}/access_tokens",
            self.api_base
        );
        let res = self
            .http
            .post(url)
            .bearer_auth(&jwt)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "git-fight")
            .json(&serde_json::json!({}))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("access_tokens {}", res.status()));
        }
        #[derive(Deserialize)]
        struct Tok {
            token: String,
            expires_at: String,
        }
        let body: Tok = json_capped(res, MAX_TOKEN_JSON)
            .await
            .ok_or_else(|| "access_tokens json".to_string())?;
        let expires = parse_expires(&body.expires_at);
        let token = body.token;
        {
            let mut cache = self.tokens.lock().await;
            drop_expired_tokens(&mut cache, Instant::now());
            cache.insert(installation_id, (token.clone(), expires));
        }
        Ok(token)
    }

    async fn authed(
        &self,
        installation_id: u64,
        method: reqwest::Method,
        path: &str,
    ) -> Result<reqwest::RequestBuilder, String> {
        let token = self.installation_token(installation_id).await?;
        Ok(self
            .http
            .request(method, format!("{}{path}", self.api_base))
            .bearer_auth(token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "git-fight"))
    }

    pub async fn get_repo(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<RepoInfo, String> {
        require_names(owner, repo)?;
        let res = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!("/repos/{owner}/{repo}"),
            )
            .await?
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("repo {}", res.status()));
        }
        json_capped(res, MAX_API_JSON)
            .await
            .ok_or_else(|| "repo json".into())
    }

    pub async fn get_pull(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullInfo, String> {
        require_names(owner, repo)?;
        let res = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!("/repos/{owner}/{repo}/pulls/{number}"),
            )
            .await?
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("pull {}", res.status()));
        }
        let pr: PullInfo = json_capped(res, MAX_API_JSON)
            .await
            .ok_or_else(|| "pull json".to_string())?;
        if !crate::gitutil::is_github_sha(&pr.head.sha)
            || !crate::gitutil::is_github_sha(&pr.base.sha)
        {
            return Err("pull sha".into());
        }
        Ok(pr)
    }

    pub async fn poll_mergeable(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullInfo, String> {
        let mut wait = self.poll_wait;
        let cap = (self.poll_wait * 8)
            .max(Duration::from_millis(1))
            .min(MERGEABLE_POLL_CAP);
        let mut last = None;
        for _ in 0..MERGEABLE_POLL_TRIES {
            let pr = self.get_pull(installation_id, owner, repo, number).await?;
            if pr.mergeable.is_some() {
                return Ok(pr);
            }
            last = Some(pr);
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(cap);
        }
        last.ok_or_else(|| "mergeable stayed null".into())
    }

    pub async fn comment(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<u64, String> {
        require_names(owner, repo)?;
        let res = self
            .authed(
                installation_id,
                reqwest::Method::POST,
                &format!("/repos/{owner}/{repo}/issues/{number}/comments"),
            )
            .await?
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("comment {}", res.status()));
        }
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        posted_comment_id(json_capped::<Id>(res, MAX_API_JSON).await.map(|c| c.id))
    }

    pub async fn edit_comment(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<u64, String> {
        require_names(owner, repo)?;
        let res = self
            .authed(
                installation_id,
                reqwest::Method::PATCH,
                &format!("/repos/{owner}/{repo}/issues/comments/{comment_id}"),
            )
            .await?
            .json(&serde_json::json!({ "body": body }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("edit_comment {}", res.status()));
        }
        Ok(comment_id)
    }

    /// Edit the original challenge comment when we have its id; otherwise post a new one.
    pub async fn issue_comment(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
        existing_id: Option<i64>,
        body: &str,
    ) -> Result<u64, String> {
        if let Some(id) = existing_id.filter(|i| *i > 0) {
            self.edit_comment(installation_id, owner, repo, id as u64, body)
                .await?;
            return Ok(id as u64);
        }
        self.comment(installation_id, owner, repo, number, body)
            .await
    }

    pub async fn auto_challenge_enabled(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        r#ref: &str,
    ) -> bool {
        if !is_safe_github_name(owner) || !is_safe_github_name(repo) {
            return false;
        }
        let path = if r#ref.is_empty() {
            format!("/repos/{owner}/{repo}/contents/.github/git-fight.yml")
        } else if !is_safe_git_ref(r#ref) {
            return false;
        } else {
            format!(
                "/repos/{owner}/{repo}/contents/.github/git-fight.yml?ref={}",
                urlencoding(r#ref)
            )
        };
        let res = self
            .authed(installation_id, reqwest::Method::GET, &path)
            .await;
        let Ok(builder) = res else {
            return false;
        };
        let Ok(resp) = builder.send().await else {
            return false;
        };
        if !resp.status().is_success() {
            return false;
        }
        #[derive(Deserialize)]
        struct File {
            #[serde(rename = "type")]
            kind: Option<String>,
            content: Option<String>,
            encoding: Option<String>,
            size: Option<u64>,
        }
        let Some(file) = json_capped::<File>(resp, MAX_CONTENTS_JSON).await else {
            return false;
        };
        if file.kind.as_deref() != Some("file") {
            return false;
        }
        if file.encoding.as_deref() != Some("base64") {
            return false;
        }
        let size = file.size.unwrap_or(u64::MAX);
        if size == 0 || size > MAX_FIGHT_YML_BYTES as u64 {
            return false;
        }
        let Some(content) = file.content else {
            return false;
        };
        if content.len() > MAX_FIGHT_YML_BYTES * 2 {
            return false;
        }
        let clean: String = content.chars().filter(|c| !c.is_whitespace()).collect();
        let Ok(raw) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, clean)
        else {
            return false;
        };
        if raw.len() > MAX_FIGHT_YML_BYTES {
            return false;
        }
        let Ok(decoded) = String::from_utf8(raw) else {
            return false;
        };
        decoded.lines().any(|l| l.trim() == "auto_challenge: true")
    }

    pub async fn login_for_commit(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        sha: &str,
    ) -> LoginLookup {
        if !is_safe_github_name(owner)
            || !is_safe_github_name(repo)
            || !crate::gitutil::is_safe_rev(sha)
        {
            return LoginLookup::None;
        }
        // List API (no `files` patches). GET /commits/{sha} can be many MB.
        let Ok(req) = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!(
                    "/repos/{owner}/{repo}/commits?sha={}&per_page=1",
                    urlencoding(sha)
                ),
            )
            .await
        else {
            return LoginLookup::Unavailable;
        };
        let Ok(res) = req.send().await else {
            return LoginLookup::Unavailable;
        };
        if !res.status().is_success() {
            // The blamed SHA should be in the repo we just cloned.
            // 404/5xx is a miss, not “no GitHub account”.
            return LoginLookup::Unavailable;
        }
        #[derive(Deserialize)]
        struct Row {
            sha: Option<String>,
            author: Option<User>,
        }
        #[derive(Deserialize)]
        struct User {
            login: Option<String>,
        }
        let Some(rows) = json_capped::<Vec<Row>>(res, MAX_COMMITS_JSON).await else {
            return LoginLookup::Unavailable;
        };
        let Some(row) = rows.into_iter().next() else {
            return LoginLookup::Unavailable;
        };
        let got = row.sha.as_deref().unwrap_or("");
        if !got.eq_ignore_ascii_case(sha) {
            return LoginLookup::Unavailable;
        }
        match row.author.and_then(|a| a.login) {
            Some(login) => match normalize_github_login(&login) {
                Some(stored) => LoginLookup::Found(stored),
                None => LoginLookup::Rejected,
            },
            None => LoginLookup::None,
        }
    }

    pub async fn login_for_email(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        email: &str,
    ) -> LoginLookup {
        if !is_safe_email(email) || !is_safe_github_name(owner) || !is_safe_github_name(repo) {
            return LoginLookup::None;
        }
        let Ok(req) = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!(
                    "/repos/{owner}/{repo}/commits?author={}&per_page=1",
                    urlencoding(email)
                ),
            )
            .await
        else {
            return LoginLookup::Unavailable;
        };
        let Ok(res) = req.send().await else {
            return LoginLookup::Unavailable;
        };
        let status = res.status();
        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::GONE {
            return LoginLookup::None;
        }
        if !status.is_success() {
            return LoginLookup::Unavailable;
        }
        #[derive(Deserialize)]
        struct Row {
            author: Option<User>,
            commit: CommitBody,
        }
        #[derive(Deserialize)]
        struct User {
            login: Option<String>,
        }
        #[derive(Deserialize)]
        struct CommitBody {
            author: Option<GitUser>,
        }
        #[derive(Deserialize)]
        struct GitUser {
            email: Option<String>,
        }
        let Some(rows) = json_capped::<Vec<Row>>(res, MAX_COMMITS_JSON).await else {
            return LoginLookup::Unavailable;
        };
        for row in rows {
            if row
                .commit
                .author
                .and_then(|a| a.email)
                .is_some_and(|got| got.eq_ignore_ascii_case(email))
            {
                if let Some(login) = row
                    .author
                    .and_then(|a| a.login)
                    .and_then(|l| normalize_github_login(&l))
                {
                    return LoginLookup::Found(login);
                }
            }
        }
        LoginLookup::None
    }

    pub async fn oauth_user(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<(i64, String), String> {
        if !is_safe_oauth_code(code) || !is_safe_oauth_code(code_verifier) {
            return Err("bad oauth".into());
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: Option<String>,
            error: Option<String>,
        }
        let res = self
            .http
            .post(format!("{}/login/oauth/access_token", self.oauth_base))
            .header("Accept", "application/json")
            .header("User-Agent", "git-fight")
            .json(&serde_json::json!({
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "code": code,
                "redirect_uri": redirect_uri,
                "code_verifier": code_verifier,
            }))
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let tok: Token = json_capped(res, MAX_TOKEN_JSON)
            .await
            .ok_or_else(|| "oauth json".to_string())?;
        if let Some(err) = tok.error {
            return Err(err);
        }
        let token = tok
            .access_token
            .ok_or_else(|| "no access_token".to_string())?;
        let user_res = self
            .http
            .get(format!("{}/user", self.api_base))
            .bearer_auth(&token)
            .header("Accept", "application/vnd.github+json")
            .header("User-Agent", "git-fight")
            .send()
            .await
            .map_err(|e| e.to_string())?;
        drop(token);
        #[derive(Deserialize)]
        struct User {
            id: i64,
            login: String,
        }
        let user: User = json_capped(user_res, MAX_TOKEN_JSON)
            .await
            .ok_or_else(|| "user json".to_string())?;
        let login = normalize_github_login(&user.login).ok_or_else(|| "bad login".to_string())?;
        Ok((user.id, login))
    }

    pub fn authorize_url(&self, redirect_uri: &str, state: &str, code_challenge: &str) -> String {
        format!(
            "{}/login/oauth/authorize?client_id={}&redirect_uri={}&state={}&allow_signup=false&code_challenge={}&code_challenge_method=S256",
            self.oauth_base,
            urlencoding(&self.client_id),
            urlencoding(redirect_uri),
            urlencoding(state),
            urlencoding(code_challenge),
        )
    }
}

async fn json_capped<T: DeserializeOwned>(resp: reqwest::Response, max: usize) -> Option<T> {
    if resp.content_length().is_some_and(|n| n > max as u64) {
        return None;
    }
    let mut acc = Vec::new();
    let mut resp = resp;
    loop {
        match resp.chunk().await {
            Ok(Some(chunk)) => {
                if acc.len().saturating_add(chunk.len()) > max {
                    return None;
                }
                acc.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return None,
        }
    }
    serde_json::from_slice(&acc).ok()
}

fn require_names(owner: &str, repo: &str) -> Result<(), String> {
    if is_safe_github_name(owner) && is_safe_github_name(repo) {
        Ok(())
    } else {
        Err("unsafe name".into())
    }
}

/// API/OAuth base URL. HTTPS anywhere, or HTTP only to loopback (wiremock).
pub fn is_safe_github_endpoint(url: &str) -> bool {
    let url = url.trim();
    if !(8..=200).contains(&url.len()) {
        return false;
    }
    if url.contains(|c: char| c.is_ascii_whitespace() || matches!(c, '\\' | '?' | '#' | '@')) {
        return false;
    }
    if let Some(rest) = url.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        return !host.is_empty() && host != "0.0.0.0" && host != "*" && !host.starts_with('-');
    }
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let hostport = rest.split('/').next().unwrap_or("");
    let host = if let Some(inner) = hostport.strip_prefix('[') {
        inner.split(']').next().unwrap_or("")
    } else {
        hostport.split(':').next().unwrap_or("")
    };
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

fn is_github_dot_com_api(url: &str) -> bool {
    url.trim().trim_end_matches('/') == "https://api.github.com"
}

fn is_github_dot_com_oauth(url: &str) -> bool {
    url.trim().trim_end_matches('/') == "https://github.com"
}

/// GitHub owner or repo name. Used in clone URLs and API paths — never a slash or host.
pub fn is_safe_github_name(s: &str) -> bool {
    let n = s.len();
    (1..=100).contains(&n)
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// GitHub logins, owners, and repos are case-insensitive. Store one spelling
/// so a fighter cannot be locked out, a second `/fight` cannot bypass the
/// one-open-match slot, and the leaderboard cannot split.
pub fn normalize_github_login(s: &str) -> Option<String> {
    let t = s.trim();
    is_safe_github_name(t).then(|| t.to_ascii_lowercase())
}

/// Fold an owner, repo, or login for storage. Empty stays empty (local demo).
pub fn fold_github_name(s: &str) -> String {
    let t = s.trim();
    if t.is_empty() {
        return String::new();
    }
    normalize_github_login(t).unwrap_or_else(|| t.to_ascii_lowercase())
}

pub fn fold_github_login_opt(s: Option<&str>) -> Option<String> {
    s.and_then(normalize_github_login)
}

pub fn same_github_login(a: Option<&str>, b: Option<&str>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) if !a.is_empty() && !b.is_empty() => a.eq_ignore_ascii_case(b),
        _ => false,
    }
}

fn is_safe_git_ref(s: &str) -> bool {
    let n = s.len();
    (1..=255).contains(&n)
        && !s.starts_with('-')
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

fn is_safe_email(s: &str) -> bool {
    let n = s.len();
    (3..=254).contains(&n)
        && s.contains('@')
        && !s.starts_with('-')
        && s.bytes().all(|b| {
            b.is_ascii_graphic() && !matches!(b, b'?' | b'&' | b'#' | b'\\' | b'"' | b'\'')
        })
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn drop_expired_tokens(cache: &mut HashMap<u64, (String, Instant)>, now: Instant) {
    cache.retain(|_, (_, exp)| now + Duration::from_secs(30) < *exp);
}

/// Authorization `code` / PKCE verifier. Cap so a junk callback cannot POST megabytes.
pub(crate) fn is_safe_oauth_code(s: &str) -> bool {
    let n = s.len();
    (1..=128).contains(&n)
        && s.bytes().all(|b| {
            b.is_ascii_graphic() && !matches!(b, b'?' | b'&' | b'#' | b'\\' | b'"' | b'\'')
        })
}

fn parse_expires(rfc: &str) -> Instant {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(rfc) {
        let secs = (dt.with_timezone(&chrono::Utc) - chrono::Utc::now()).num_seconds();
        return Instant::now() + Duration::from_secs(secs.max(60) as u64);
    }
    Instant::now() + Duration::from_secs(50 * 60)
}

#[derive(Clone, Debug, Deserialize)]
pub struct RepoInfo {
    pub size: u64,
    #[serde(default)]
    pub default_branch: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullInfo {
    pub number: u64,
    pub mergeable: Option<bool>,
    pub head: ShaRef,
    pub base: ShaRef,
    pub user: UserInfo,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShaRef {
    pub sha: String,
    #[serde(default)]
    pub r#ref: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct UserInfo {
    pub login: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

    #[test]
    fn live_mergeable_poll_waits_for_github() {
        assert!(MERGEABLE_POLL_WAIT >= Duration::from_secs(1));
        assert!(MERGEABLE_POLL_CAP >= Duration::from_secs(4));
        assert_eq!(MERGEABLE_POLL_TRIES, 8);
    }

    #[test]
    fn github_endpoints_https_or_loopback_http() {
        assert!(is_safe_github_endpoint("https://api.github.com"));
        assert!(is_safe_github_endpoint("https://github.com"));
        assert!(is_safe_github_endpoint("http://127.0.0.1:1234"));
        assert!(is_safe_github_endpoint("http://localhost/"));
        assert!(!is_safe_github_endpoint("http://evil.example"));
        assert!(!is_safe_github_endpoint("http://api.github.com"));
        assert!(!is_safe_github_endpoint("https://evil@api.github.com"));
        assert!(!is_safe_github_endpoint("ftp://api.github.com"));
        assert!(is_github_dot_com_api("https://api.github.com"));
        assert!(is_github_dot_com_oauth("https://github.com"));
        assert!(!is_github_dot_com_api("https://evil.example"));
        assert!(!is_github_dot_com_oauth("https://api.github.com"));
        assert!(!is_github_dot_com_api("http://api.github.com"));
    }

    #[test]
    fn expired_install_tokens_leave_the_cache() {
        let mut cache = HashMap::new();
        let now = Instant::now();
        cache.insert(1, ("live".into(), now + Duration::from_secs(3600)));
        cache.insert(2, ("dead".into(), now));
        drop_expired_tokens(&mut cache, now);
        assert_eq!(cache.len(), 1);
        assert_eq!(cache.get(&1).map(|(t, _)| t.as_str()), Some("live"));
    }

    #[test]
    fn github_logins_fold_case() {
        assert_eq!(normalize_github_login("Alice").as_deref(), Some("alice"));
        assert_eq!(normalize_github_login("  BOB  ").as_deref(), Some("bob"));
        assert!(normalize_github_login("").is_none());
        assert!(normalize_github_login("../x").is_none());
        assert!(same_github_login(Some("Alice"), Some("alice")));
        assert!(same_github_login(Some("BOB"), Some("bob")));
        assert!(!same_github_login(Some("alice"), Some("bob")));
        assert!(!same_github_login(Some("alice"), Some("")));
        assert!(!same_github_login(None, Some("alice")));
        assert_eq!(fold_github_name("Acme"), "acme");
        assert_eq!(fold_github_name(""), "");
        assert_eq!(fold_github_login_opt(Some("BOB")).as_deref(), Some("bob"));
        assert!(fold_github_login_opt(Some("../x")).is_none());
        assert!(fold_github_login_opt(Some("not a login")).is_none());
        assert!(fold_github_login_opt(Some("")).is_none());
    }

    #[test]
    fn issue_comment_with_id_retries_edit_only() {
        assert_eq!(issue_comment_followup(true, Ok(())), "edit");
        assert_eq!(
            issue_comment_followup(true, Err(())),
            "retry",
            "a failed PATCH must not POST a second outcome thread"
        );
        assert_eq!(issue_comment_followup(false, Err(())), "post");
    }

    #[test]
    fn truncated_comment_json_is_not_a_zero_id() {
        assert_eq!(posted_comment_id(Some(9)).unwrap(), 9);
        assert!(posted_comment_id(Some(0)).is_err());
        assert!(posted_comment_id(None).is_err());
    }

    fn issue_comment_followup(existing: bool, edit: Result<(), ()>) -> &'static str {
        if existing {
            match edit {
                Ok(()) => "edit",
                Err(()) => "retry",
            }
        } else {
            "post"
        }
    }

    #[test]
    fn oauth_code_is_short_and_printable() {
        assert!(is_safe_oauth_code("abc"));
        assert!(is_safe_oauth_code(&"a".repeat(64)));
        assert!(!is_safe_oauth_code(""));
        assert!(!is_safe_oauth_code(&"a".repeat(129)));
        assert!(!is_safe_oauth_code("code with space"));
        assert!(!is_safe_oauth_code("x&redirect=https://evil"));
    }
}
