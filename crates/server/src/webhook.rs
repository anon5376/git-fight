//! GitHub webhooks. Signature first, then JSON.

use crate::challenge::{self, ChallengeCtx};
use crate::db;
use crate::limits::WEBHOOK_MAX_AGE_SECS;
use crate::sig;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode as HttpStatus;
use chrono::Utc;
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// GitHub's `X-GitHub-Delivery` is a UUID. Reject junk so the PK cannot be a path.
fn is_delivery_id(s: &str) -> bool {
    let n = s.len();
    (1..=128).contains(&n) && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn payload_hash(body: &[u8]) -> String {
    hex::encode(Sha256::digest(body))
}

/// GitHub HMAC has no timestamp. Events older than a match (or unparseable) are ignored.
fn timestamp_is_fresh(ts: Option<&str>) -> bool {
    let Some(raw) = ts.map(str::trim).filter(|s| !s.is_empty()) else {
        return false;
    };
    let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) else {
        return false;
    };
    let age = Utc::now().timestamp() - dt.timestamp();
    (-300..=WEBHOOK_MAX_AGE_SECS).contains(&age)
}

pub async fn github_webhook(State(state): State<crate::app::AppState>, req: Request) -> HttpStatus {
    let Some(secret) = state.webhook_secret.as_ref() else {
        return HttpStatus::SERVICE_UNAVAILABLE;
    };
    let (parts, body) = req.into_parts();
    let Ok(body) = to_bytes(body, 2 * 1024 * 1024).await else {
        return HttpStatus::PAYLOAD_TOO_LARGE;
    };
    let sig = parts
        .headers
        .get("X-Hub-Signature-256")
        .or_else(|| parts.headers.get("x-hub-signature-256"))
        .and_then(|v| v.to_str().ok());
    if !sig::verify_signature(secret, &body, sig) {
        return HttpStatus::UNAUTHORIZED;
    }

    let event = parts
        .headers
        .get("X-GitHub-Event")
        .or_else(|| parts.headers.get("x-github-event"))
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let delivery = parts
        .headers
        .get("X-GitHub-Delivery")
        .or_else(|| parts.headers.get("x-github-delivery"))
        .and_then(|v| v.to_str().ok())
        .filter(|id| is_delivery_id(id));
    let Some(id) = delivery else {
        return HttpStatus::BAD_REQUEST;
    };
    match db::record_delivery(&state.pool, id, &payload_hash(&body)).await {
        Ok(false) => return HttpStatus::OK,
        Ok(true) => {}
        Err(_) => return HttpStatus::INTERNAL_SERVER_ERROR,
    }

    let payload: Hook = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return HttpStatus::BAD_REQUEST,
    };

    match event {
        "issue_comment" => handle_comment(&state, payload).await,
        "pull_request" => handle_pull(&state, payload).await,
        _ => HttpStatus::OK,
    }
}

async fn handle_comment(state: &crate::app::AppState, hook: Hook) -> HttpStatus {
    if hook.action.as_deref() != Some("created") && hook.action.as_deref() != Some("edited") {
        return HttpStatus::OK;
    }
    let Some(comment) = &hook.comment else {
        return HttpStatus::OK;
    };
    if !timestamp_is_fresh(
        comment
            .updated_at
            .as_deref()
            .or(comment.created_at.as_deref()),
    ) {
        return HttpStatus::OK;
    }
    if !challenge::is_fight_comment(&comment.body) {
        return HttpStatus::OK;
    }
    if challenge::is_bot_user(comment.user.r#type.as_deref(), Some(&comment.user.login))
        || challenge::is_bot_user(
            hook.sender.as_ref().and_then(|s| s.r#type.as_deref()),
            hook.sender.as_ref().map(|s| s.login.as_str()),
        )
    {
        return HttpStatus::OK;
    }
    let Some(issue) = &hook.issue else {
        return HttpStatus::OK;
    };
    if issue.pull_request.is_none() {
        return HttpStatus::OK;
    }
    spawn_challenge(state, &hook, issue.number).await
}

async fn handle_pull(state: &crate::app::AppState, hook: Hook) -> HttpStatus {
    let action = hook.action.as_deref().unwrap_or("");
    if !matches!(action, "opened" | "reopened" | "synchronize" | "edited") {
        return HttpStatus::OK;
    }
    let Some(pr) = &hook.pull_request else {
        return HttpStatus::OK;
    };
    if !timestamp_is_fresh(pr.updated_at.as_deref()) {
        return HttpStatus::OK;
    }
    let Some(repo) = &hook.repository else {
        return HttpStatus::OK;
    };
    if !crate::gh::is_safe_github_name(&repo.owner.login)
        || !crate::gh::is_safe_github_name(&repo.name)
    {
        return HttpStatus::OK;
    }
    if let Ok(Some(row)) =
        db::open_match_for_pr(&state.pool, &repo.owner.login, &repo.name, pr.number).await
    {
        notice_if_outdated(state, &row, pr).await;
        return HttpStatus::OK;
    }
    if !matches!(action, "opened" | "reopened" | "synchronize") {
        return HttpStatus::OK;
    }
    let Some(inst) = hook.installation.as_ref().map(|i| i.id) else {
        return HttpStatus::OK;
    };
    let Some(gh) = &state.github else {
        return HttpStatus::OK;
    };
    if !gh
        .auto_challenge_enabled(inst, &repo.owner.login, &repo.name, &repo.default_branch)
        .await
    {
        return HttpStatus::OK;
    }
    spawn_challenge(state, &hook, pr.number).await
}

async fn notice_if_outdated(state: &crate::app::AppState, row: &db::MatchRow, pr: &Pr) {
    if row.abort_reason.as_deref() == Some("outdated") {
        return;
    }
    let Some(head) = pr
        .head
        .as_ref()
        .map(|s| s.sha.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let Some(base) = pr
        .base
        .as_ref()
        .map(|s| s.sha.as_str())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    if head.eq_ignore_ascii_case(&row.pr_head_sha) && base.eq_ignore_ascii_case(&row.pr_base_sha) {
        return;
    }
    if !db::abort_open_match(&state.pool, &row.id, "outdated")
        .await
        .unwrap_or(false)
    {
        return;
    }
    state.close_room(&row.id).await;
    let Some(inst) = row.installation_id.filter(|i| *i > 0).map(|i| i as u64) else {
        return;
    };
    let Some(gh) = &state.github else {
        return;
    };
    if row.pr_number <= 0 {
        return;
    }
    let public = state.auth.public_url.trim_end_matches('/');
    let body = format!(
        "git fight: this fight used outdated code (PR head or base moved). Nothing will be pushed. Comment `/fight` for a rematch.\nopen match: {public}/match/{}",
        row.id
    );
    let _ = gh
        .issue_comment(
            inst,
            &row.owner,
            &row.repo,
            row.pr_number as u64,
            row.challenge_comment_id,
            &body,
        )
        .await;
}

async fn spawn_challenge(state: &crate::app::AppState, hook: &Hook, number: u64) -> HttpStatus {
    let (Some(repo), Some(inst), Some(gh)) = (
        hook.repository.as_ref(),
        hook.installation.as_ref().map(|i| i.id),
        state.github.clone(),
    ) else {
        return HttpStatus::OK;
    };
    let ctx = ChallengeCtx {
        gh,
        pool: state.pool.clone(),
        public_url: state.auth.public_url.clone(),
        test_repos: state.test_repos.clone(),
        expire_secs: state.config.expire_secs,
    };
    let owner = crate::gh::fold_github_name(&repo.owner.login);
    let name = crate::gh::fold_github_name(&repo.name);
    if !crate::gh::is_safe_github_name(&owner) || !crate::gh::is_safe_github_name(&name) {
        return HttpStatus::OK;
    }
    tokio::spawn(async move {
        let start = match challenge::start_challenge(&ctx, inst, &owner, &name, number).await {
            Ok(msg) => msg,
            Err(_) => challenge::ChallengeStart {
                body: "git fight could not start".into(),
                match_id: None,
            },
        };
        if start.body.is_empty() {
            return;
        }
        let posted = ctx
            .gh
            .comment(inst, &owner, &name, number, &start.body)
            .await
            .unwrap_or(0);
        if let Some(match_id) = start.match_id {
            if posted > 0 {
                let _ = db::set_challenge_comment_id(&ctx.pool, &match_id, posted as i64).await;
            }
        }
    });
    HttpStatus::OK
}

#[derive(Deserialize)]
struct Hook {
    action: Option<String>,
    installation: Option<Inst>,
    repository: Option<Repo>,
    issue: Option<Issue>,
    comment: Option<Comment>,
    sender: Option<User>,
    pull_request: Option<Pr>,
}

#[derive(Deserialize)]
struct Inst {
    id: u64,
}

#[derive(Deserialize)]
struct Repo {
    name: String,
    owner: User,
    #[serde(default)]
    default_branch: String,
}

#[derive(Deserialize)]
struct User {
    login: String,
    #[serde(rename = "type")]
    r#type: Option<String>,
}

#[derive(Deserialize)]
struct Issue {
    number: u64,
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Comment {
    body: String,
    user: User,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct Pr {
    number: u64,
    #[serde(default)]
    head: Option<Sha>,
    #[serde(default)]
    base: Option<Sha>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Deserialize)]
struct Sha {
    sha: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_timestamps_must_be_fresh() {
        assert!(timestamp_is_fresh(Some(&Utc::now().to_rfc3339())));
        assert!(!timestamp_is_fresh(None));
        assert!(!timestamp_is_fresh(Some("")));
        assert!(!timestamp_is_fresh(Some("not-a-date")));
        assert!(!timestamp_is_fresh(Some("2000-01-01T00:00:00Z")));
    }
}
