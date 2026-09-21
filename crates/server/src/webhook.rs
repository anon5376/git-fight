//! GitHub webhooks. Signature first, then JSON.

use crate::challenge::{self, ChallengeCtx};
use crate::db;
use crate::sig;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::StatusCode as HttpStatus;
use serde::Deserialize;

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
        .and_then(|v| v.to_str().ok());

    if let Some(id) = delivery {
        match db::record_delivery(&state.pool, id).await {
            Ok(false) => return HttpStatus::OK,
            Ok(true) => {}
            Err(_) => return HttpStatus::INTERNAL_SERVER_ERROR,
        }
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
    if !matches!(action, "opened" | "reopened" | "synchronize") {
        return HttpStatus::OK;
    }
    let Some(pr) = &hook.pull_request else {
        return HttpStatus::OK;
    };
    let Some(repo) = &hook.repository else {
        return HttpStatus::OK;
    };
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
    let owner = repo.owner.login.clone();
    let name = repo.name.clone();
    let body = match challenge::start_challenge(&ctx, inst, &owner, &name, number).await {
        Ok(msg) => msg,
        Err(e) => format!("git fight could not start: {e}"),
    };
    let _ = ctx.gh.comment(inst, &owner, &name, number, &body).await;
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
}

#[derive(Deserialize)]
struct Pr {
    number: u64,
}
