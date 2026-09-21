//! GitHub App HTTP + in-memory installation tokens.

use crate::limits::MAX_FIGHT_YML_BYTES;
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
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

/// Live GitHub often leaves `mergeable` null for seconds while it computes.
const MERGEABLE_POLL_WAIT: Duration = Duration::from_secs(1);
const MERGEABLE_POLL_CAP: Duration = Duration::from_secs(4);
const MERGEABLE_POLL_TRIES: u32 = 8;
/// Contents JSON for a 4 KiB yaml plus GitHub metadata. Bigger is not a config.
const MAX_CONTENTS_JSON: usize = 16 * 1024;

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
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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
            let cache = self.tokens.lock().await;
            if let Some((tok, exp)) = cache.get(&installation_id) {
                if Instant::now() + Duration::from_secs(30) < *exp {
                    return Ok(tok.clone());
                }
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
        let body: Tok = res.json().await.map_err(|e| e.to_string())?;
        let expires = parse_expires(&body.expires_at);
        let token = body.token;
        self.tokens
            .lock()
            .await
            .insert(installation_id, (token.clone(), expires));
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
        res.json().await.map_err(|e| e.to_string())
    }

    pub async fn get_pull(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<PullInfo, String> {
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
        res.json().await.map_err(|e| e.to_string())
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
        Ok(res.json::<Id>().await.map(|c| c.id).unwrap_or(0))
    }

    pub async fn edit_comment(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        comment_id: u64,
        body: &str,
    ) -> Result<u64, String> {
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
            if self
                .edit_comment(installation_id, owner, repo, id as u64, body)
                .await
                .is_ok()
            {
                return Ok(id as u64);
            }
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
        if resp
            .content_length()
            .is_some_and(|n| n > MAX_CONTENTS_JSON as u64)
        {
            return false;
        }
        let Ok(bytes) = resp.bytes().await else {
            return false;
        };
        if bytes.len() > MAX_CONTENTS_JSON {
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
        let Ok(file) = serde_json::from_slice::<File>(&bytes) else {
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
    ) -> Option<String> {
        if !is_safe_github_name(owner)
            || !is_safe_github_name(repo)
            || !crate::gitutil::is_safe_rev(sha)
        {
            return None;
        }
        let res = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!("/repos/{owner}/{repo}/commits/{sha}"),
            )
            .await
            .ok()?
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
        }
        #[derive(Deserialize)]
        struct Commit {
            author: Option<User>,
        }
        #[derive(Deserialize)]
        struct User {
            login: Option<String>,
        }
        res.json::<Commit>()
            .await
            .ok()
            .and_then(|c| c.author.and_then(|a| a.login))
    }

    pub async fn login_for_email(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        email: &str,
    ) -> Option<String> {
        if !is_safe_email(email) || !is_safe_github_name(owner) || !is_safe_github_name(repo) {
            return None;
        }
        let res = self
            .authed(
                installation_id,
                reqwest::Method::GET,
                &format!(
                    "/repos/{owner}/{repo}/commits?author={}&per_page=1",
                    urlencoding(email)
                ),
            )
            .await
            .ok()?
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
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
        let rows: Vec<Row> = res.json().await.ok()?;
        for row in rows {
            if row.commit.author.and_then(|a| a.email).as_deref() == Some(email) {
                if let Some(login) = row.author.and_then(|a| a.login) {
                    return Some(login);
                }
            }
        }
        None
    }

    pub async fn oauth_user(
        &self,
        code: &str,
        redirect_uri: &str,
        code_verifier: &str,
    ) -> Result<(i64, String), String> {
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
        let tok: Token = res.json().await.map_err(|e| e.to_string())?;
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
        let user: User = user_res.json().await.map_err(|e| e.to_string())?;
        Ok((user.id, user.login))
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

/// GitHub owner or repo name. Used in clone URLs and API paths — never a slash or host.
pub fn is_safe_github_name(s: &str) -> bool {
    let n = s.len();
    (1..=100).contains(&n)
        && !s.contains("..")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
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

    #[test]
    fn live_mergeable_poll_waits_for_github() {
        assert!(MERGEABLE_POLL_WAIT >= Duration::from_secs(1));
        assert!(MERGEABLE_POLL_CAP >= Duration::from_secs(4));
        assert_eq!(MERGEABLE_POLL_TRIES, 8);
    }
}
