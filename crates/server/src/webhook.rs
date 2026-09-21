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
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// `synchronize` lookups that exhausted the short retry loop. Restart loses
/// this map; the next webhook or 24h expiry covers leftover rows.
#[derive(Clone, Default)]
pub(crate) struct LookupTrack {
    pending: Arc<std::sync::Mutex<HashMap<String, PendingPull>>>,
}

#[derive(Clone)]
struct PendingPull {
    owner: String,
    repo: String,
    pr: Pr,
}

fn lookup_key(owner: &str, repo: &str, number: u64) -> String {
    format!(
        "{}/{}/{}",
        crate::gh::fold_github_name(owner),
        crate::gh::fold_github_name(repo),
        number
    )
}

impl LookupTrack {
    fn queue(&self, owner: String, repo: String, pr: Pr) {
        if let Ok(mut g) = self.pending.lock() {
            let key = lookup_key(&owner, &repo, pr.number);
            g.insert(key, PendingPull { owner, repo, pr });
        }
    }

    fn snapshot(&self) -> Vec<PendingPull> {
        self.pending
            .lock()
            .map(|g| g.values().cloned().collect())
            .unwrap_or_default()
    }

    fn dequeue(&self, owner: &str, repo: &str, number: u64) {
        if let Ok(mut g) = self.pending.lock() {
            g.remove(&lookup_key(owner, repo, number));
        }
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.pending.lock().map(|g| g.is_empty()).unwrap_or(true)
    }
}

#[cfg(test)]
pub(crate) fn enqueue_lookup_for_test(
    state: &crate::app::AppState,
    owner: &str,
    repo: &str,
    number: u64,
    head: &str,
    base: &str,
) {
    state.lookups.queue(
        owner.into(),
        repo.into(),
        Pr {
            number,
            head: Some(Sha { sha: head.into() }),
            base: Some(Sha { sha: base.into() }),
            updated_at: None,
        },
    );
}

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
    match open_match_lookup_retry(&state.pool, &repo.owner.login, &repo.name, pr.number).await {
        Ok(Some(row)) => {
            notice_if_outdated(state, &row, pr).await;
            return HttpStatus::OK;
        }
        Ok(None) => {}
        Err(_) => {
            // Busy lookup is not "no open match": retry notice, do not
            // auto_challenge while an open row may still exist.
            schedule_open_lookup(
                state.clone(),
                repo.owner.login.clone(),
                repo.name.clone(),
                pr.clone(),
            );
            return HttpStatus::OK;
        }
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
        .filter(|s| crate::gitutil::is_github_sha(s))
    else {
        return;
    };
    let Some(base) = pr
        .base
        .as_ref()
        .map(|s| s.sha.as_str())
        .filter(|s| crate::gitutil::is_github_sha(s))
    else {
        return;
    };
    if head.eq_ignore_ascii_case(&row.pr_head_sha) && base.eq_ignore_ascii_case(&row.pr_base_sha) {
        return;
    }
    match db::abort_open_retry(&state.pool, &row.id, "outdated").await {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            // Drift is known. Stop lockstep now; keep retrying abort so
            // rematch `/fight` is not stuck on a leftover open row.
            state.mark_closing(&row.id);
            state.close_room(&row.id).await;
            schedule_outdated_abort(state.clone(), row.clone());
            return;
        }
    }
    close_and_comment_outdated(state, row).await;
}

fn schedule_outdated_abort(state: crate::app::AppState, row: db::MatchRow) {
    tokio::spawn(async move {
        state.mark_closing(&row.id);
        state.close_room(&row.id).await;
        for delay_ms in [25_u64, 50, 100, 200, 400, 800, 1600] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            match db::abort_open_match(&state.pool, &row.id, "outdated").await {
                Ok(true) => {
                    state.unmark_closing(&row.id);
                    close_and_comment_outdated(&state, &row).await;
                    return;
                }
                Ok(false) => {
                    state.unmark_closing(&row.id);
                    return;
                }
                Err(_) => {}
            }
        }
        // Short retry exhausted. The 5s expirer drains pending_aborts so a
        // leftover open row cannot hold matches_one_open_per_pr.
        state.queue_abort(row.id, "outdated".into());
    });
}

async fn close_and_comment_outdated(state: &crate::app::AppState, row: &db::MatchRow) {
    state.close_room(&row.id).await;
    if crate::result::comment_outdated(&state.result_ctx(), row)
        .await
        .is_err()
    {
        state.queue_abort(row.id.clone(), "outdated".into());
    }
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
        comments: state.comments.clone(),
        start_notes: state.start_notes.clone(),
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
        if let Some(ref match_id) = start.match_id {
            match open_for_comment_retry(&ctx.pool, match_id).await {
                Ok(true) => {}
                Ok(false) => {
                    ctx.comments.unmark(match_id);
                    return;
                }
                Err(_) => {
                    schedule_challenge_comment(ctx, inst, owner, name, number, start);
                    return;
                }
            }
        }
        post_challenge_comment(&ctx, inst, &owner, &name, number, &start).await;
    });
    HttpStatus::OK
}

/// Retry once. `Err` is still unknown — not "no open match".
async fn open_match_lookup_retry(
    pool: &sqlx::SqlitePool,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Option<db::MatchRow>, sqlx::Error> {
    match db::open_match_for_pr(pool, owner, repo, number).await {
        Ok(v) => Ok(v),
        Err(_) => db::open_match_for_pr(pool, owner, repo, number).await,
    }
}

fn schedule_open_lookup(state: crate::app::AppState, owner: String, repo: String, pr: Pr) {
    tokio::spawn(async move {
        for delay_ms in [25_u64, 50, 100, 200, 400, 800, 1600] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            match db::open_match_for_pr(&state.pool, &owner, &repo, pr.number).await {
                Ok(Some(row)) => {
                    notice_if_outdated(&state, &row, &pr).await;
                    return;
                }
                Ok(None) => return,
                Err(_) => {}
            }
        }
        // Short retry exhausted. The 5s expirer keeps looking so SHA-drift
        // cannot sit silent until 24h.
        state.lookups.queue(owner, repo, pr);
    });
}

pub(crate) async fn retry_pending_lookups(state: &crate::app::AppState) {
    for item in state.lookups.snapshot() {
        match db::open_match_for_pr(&state.pool, &item.owner, &item.repo, item.pr.number).await {
            Ok(Some(row)) => {
                state
                    .lookups
                    .dequeue(&item.owner, &item.repo, item.pr.number);
                notice_if_outdated(state, &row, &item.pr).await;
            }
            Ok(None) => {
                state
                    .lookups
                    .dequeue(&item.owner, &item.repo, item.pr.number);
            }
            Err(_) => {}
        }
    }
}

/// Retry once. `Ok(false)` means the row is already closed — do not post
/// a fight link. `Err` is still unknown.
pub(crate) async fn open_for_comment_retry(
    pool: &sqlx::SqlitePool,
    id: &str,
) -> Result<bool, sqlx::Error> {
    match db::is_open_match(pool, id).await {
        Ok(v) => Ok(v),
        Err(_) => db::is_open_match(pool, id).await,
    }
}

fn schedule_challenge_comment(
    ctx: ChallengeCtx,
    inst: u64,
    owner: String,
    name: String,
    number: u64,
    start: challenge::ChallengeStart,
) {
    tokio::spawn(async move {
        for delay_ms in [25_u64, 50, 100, 200, 400, 800, 1600] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            let Some(ref match_id) = start.match_id else {
                return;
            };
            match db::is_open_match(&ctx.pool, match_id).await {
                Ok(true) => {
                    post_challenge_comment(&ctx, inst, &owner, &name, number, &start).await;
                    return;
                }
                Ok(false) => {
                    ctx.comments.unmark(match_id);
                    return;
                }
                Err(_) => {}
            }
        }
        if let Some(ref match_id) = start.match_id {
            ctx.comments.unmark(match_id);
        }
    });
}

async fn post_challenge_comment(
    ctx: &ChallengeCtx,
    inst: u64,
    owner: &str,
    name: &str,
    number: u64,
    start: &challenge::ChallengeStart,
) {
    let posted = ctx
        .gh
        .comment(inst, owner, name, number, &start.body)
        .await
        .unwrap_or(0);
    if let Some(match_id) = start.match_id.as_deref() {
        crate::challenge::persist_challenge_comment(&ctx.pool, &ctx.comments, match_id, posted)
            .await;
    }
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

#[derive(Clone, Deserialize)]
struct Pr {
    number: u64,
    #[serde(default)]
    head: Option<Sha>,
    #[serde(default)]
    base: Option<Sha>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Clone, Deserialize)]
struct Sha {
    sha: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abort_retry_does_not_treat_sql_error_as_already_closed() {
        assert!(matches!(first_or_retry(Err(()), Ok(true)), Ok(true)));
        assert!(matches!(first_or_retry(Err(()), Ok(false)), Ok(false)));
        assert!(
            first_or_retry(Err(()), Err(())).is_err(),
            "two busy writes must retry later, not skip close_room"
        );
        assert!(matches!(first_or_retry(Ok(true), Err(())), Ok(true)));
    }

    fn first_or_retry(first: Result<bool, ()>, retry: Result<bool, ()>) -> Result<bool, ()> {
        first.or(retry)
    }

    #[test]
    fn pull_lookup_err_does_not_auto_challenge() {
        assert_eq!(pull_lookup_followup(Ok(Some(()))), "notice");
        assert_eq!(pull_lookup_followup(Ok(None)), "maybe_challenge");
        assert_eq!(
            pull_lookup_followup(Err(())),
            "retry",
            "busy open_match_for_pr must not fall through to spawn_challenge"
        );
    }

    fn pull_lookup_followup(result: Result<Option<()>, ()>) -> &'static str {
        match result {
            Ok(Some(())) => "notice",
            Ok(None) => "maybe_challenge",
            Err(()) => "retry",
        }
    }

    #[test]
    fn lookup_exhaust_is_queued_for_expirer() {
        assert_eq!(lookup_exhaust_followup(Ok(Some(()))), "notice");
        assert_eq!(lookup_exhaust_followup(Ok(None)), "stop");
        assert_eq!(
            lookup_exhaust_followup(Err(())),
            "queue",
            "busy open_match_for_pr after the short loop must not go silent"
        );
    }

    fn lookup_exhaust_followup(result: Result<Option<()>, ()>) -> &'static str {
        match result {
            Ok(Some(())) => "notice",
            Ok(None) => "stop",
            Err(()) => "queue",
        }
    }

    #[test]
    fn fight_link_is_not_posted_after_the_row_closes() {
        assert_eq!(fight_link_followup(false, Err(())), "post");
        assert_eq!(fight_link_followup(true, Ok(true)), "post");
        assert_eq!(fight_link_followup(true, Ok(false)), "skip");
        assert_eq!(
            fight_link_followup(true, Err(())),
            "retry",
            "busy open-status must not post a fight link we cannot confirm"
        );
    }

    fn fight_link_followup(has_match: bool, open: Result<bool, ()>) -> &'static str {
        if !has_match {
            return "post";
        }
        match open {
            Ok(true) => "post",
            Ok(false) => "skip",
            Err(()) => "retry",
        }
    }

    #[test]
    fn drift_known_closes_room_while_abort_retries() {
        assert_eq!(drift_abort_followup(Ok(true)), "close_and_comment");
        assert_eq!(drift_abort_followup(Ok(false)), "stop");
        assert_eq!(
            drift_abort_followup(Err(())),
            "close_room_and_retry",
            "persistent abort Err must still stop lockstep"
        );
    }

    fn drift_abort_followup(result: Result<bool, ()>) -> &'static str {
        match result {
            Ok(true) => "close_and_comment",
            Ok(false) => "stop",
            Err(()) => "close_room_and_retry",
        }
    }

    #[test]
    fn expirer_fight_link_rechecks_open_immediately_before_post() {
        assert_eq!(expirer_post_followup(Ok(true)), "post");
        assert_eq!(expirer_post_followup(Ok(false)), "skip");
        assert_eq!(
            expirer_post_followup(Err(())),
            "skip",
            "busy final open-status must not POST a fight link"
        );
    }

    fn expirer_post_followup(open: Result<bool, ()>) -> &'static str {
        match open {
            Ok(true) => "post",
            Ok(false) | Err(()) => "skip",
        }
    }

    #[test]
    fn webhook_timestamps_must_be_fresh() {
        assert!(timestamp_is_fresh(Some(&Utc::now().to_rfc3339())));
        assert!(!timestamp_is_fresh(None));
        assert!(!timestamp_is_fresh(Some("")));
        assert!(!timestamp_is_fresh(Some("not-a-date")));
        assert!(!timestamp_is_fresh(Some("2000-01-01T00:00:00Z")));
    }
}
