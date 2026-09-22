//! Start a match from a conflicted pull request.

use crate::db::{self, NewMatch};
use crate::gh::GitHub;
use crate::gitutil;
use crate::limits::{GIT_JOB_TIMEOUT, MAX_HUNKS, MAX_REPO_KB};
use crate::protocol::INPUT_DELAY;
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::time::timeout;

/// In-flight fight-link posts and comment ids that posted but have not
/// been stored. Restart loses this; the expirer posts leftover rows.
#[derive(Clone, Default)]
pub struct CommentTrack {
    inflight: Arc<std::sync::Mutex<HashSet<String>>>,
    pending_ids: Arc<std::sync::Mutex<HashMap<String, i64>>>,
}

impl CommentTrack {
    pub fn mark(&self, id: &str) {
        if let Ok(mut g) = self.inflight.lock() {
            g.insert(id.to_string());
        }
    }

    pub fn unmark(&self, id: &str) {
        if let Ok(mut g) = self.inflight.lock() {
            g.remove(id);
        }
    }

    pub fn is_inflight(&self, id: &str) -> bool {
        self.inflight
            .lock()
            .map(|g| g.contains(id))
            .unwrap_or(false)
    }

    pub fn queue_id(&self, id: String, comment_id: i64) {
        if comment_id <= 0 {
            return;
        }
        if let Ok(mut g) = self.pending_ids.lock() {
            g.insert(id, comment_id);
        }
    }

    pub fn pending_snapshot(&self) -> Vec<(String, i64)> {
        self.pending_ids
            .lock()
            .map(|g| g.iter().map(|(k, v)| (k.clone(), *v)).collect())
            .unwrap_or_default()
    }

    pub fn dequeue_id(&self, id: &str) {
        if let Ok(mut g) = self.pending_ids.lock() {
            g.remove(id);
        }
    }

    pub fn has_pending(&self, id: &str) -> bool {
        self.pending_ids
            .lock()
            .map(|g| g.contains_key(id))
            .unwrap_or(false)
    }
}

/// Decision abort comments that could not be posted after a busy SQLite
/// write. Restart loses this; the 120s hunk-less backstop still frees `/fight`.
#[derive(Clone, Default)]
pub struct StartNoteTrack {
    pending: Arc<std::sync::Mutex<HashMap<String, (String, String)>>>,
}

impl StartNoteTrack {
    pub(crate) fn queue(&self, id: String, reason: String, body: String) {
        if let Ok(mut g) = self.pending.lock() {
            g.insert(id, (reason, body));
        }
    }

    pub(crate) fn snapshot(&self) -> Vec<(String, String, String)> {
        self.pending
            .lock()
            .map(|g| {
                g.iter()
                    .map(|(k, (r, b))| (k.clone(), r.clone(), b.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) fn dequeue(&self, id: &str) {
        if let Ok(mut g) = self.pending.lock() {
            g.remove(id);
        }
    }
}

/// After `GitHub::comment`: set the id, hold inflight (2xx with no id),
/// or unmark so a failed POST can retry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PersistPosted {
    Set(u64),
    Hold,
    Retry,
}

pub(crate) fn persist_posted_followup(posted: Result<u64, &str>) -> PersistPosted {
    match posted {
        Ok(id) if id > 0 => PersistPosted::Set(id),
        Ok(_) => PersistPosted::Hold,
        Err("comment id") => PersistPosted::Hold,
        Err(_) => PersistPosted::Retry,
    }
}

/// Store a posted fight-link id, or queue it so the expirer SETs instead
/// of POSTing a second comment.
pub(crate) async fn persist_challenge_comment(
    pool: &SqlitePool,
    comments: &CommentTrack,
    match_id: &str,
    posted: Result<u64, String>,
) {
    match persist_posted_followup(posted.as_ref().map(|id| *id).map_err(|e| e.as_str())) {
        PersistPosted::Hold => {}
        PersistPosted::Retry => comments.unmark(match_id),
        PersistPosted::Set(posted) => {
            persist_challenge_comment_id(pool, comments, match_id, posted).await;
        }
    }
}

/// `done` = id is durable. `retry` = busy SET. `queue` = still NULL.
pub(crate) fn persist_id_followup(
    set: Result<bool, ()>,
    stored: Result<Option<i64>, ()>,
) -> &'static str {
    if set == Ok(true) || matches!(stored, Ok(Some(id)) if id > 0) {
        "done"
    } else if set.is_err() {
        "retry"
    } else {
        "queue"
    }
}

async fn persist_challenge_comment_id(
    pool: &SqlitePool,
    comments: &CommentTrack,
    match_id: &str,
    posted: u64,
) {
    let set = match db::set_challenge_comment_id(pool, match_id, posted as i64).await {
        Ok(v) => Ok(v),
        Err(_) => db::set_challenge_comment_id(pool, match_id, posted as i64)
            .await
            .map_err(|_| ()),
    };
    let stored = if set == Ok(true) {
        Ok(Some(posted as i64))
    } else {
        match db::get_match(pool, match_id).await {
            Ok(Some(row)) => Ok(row.challenge_comment_id),
            Ok(None) => Ok(None),
            Err(_) => Err(()),
        }
    };
    if persist_id_followup(set, stored) == "done" {
        comments.unmark(match_id);
    } else {
        comments.queue_id(match_id.to_string(), posted as i64);
        comments.unmark(match_id);
    }
}

#[derive(Clone)]
pub struct ChallengeCtx {
    pub gh: GitHub,
    pub pool: SqlitePool,
    pub public_url: String,
    pub test_repos: HashMap<String, PathBuf>,
    pub expire_secs: i64,
    pub comments: CommentTrack,
    pub start_notes: StartNoteTrack,
    /// Same latch as `AppState.closing`. Poisoned lock blocks the link.
    pub closing: Arc<Mutex<HashSet<String>>>,
}

impl ChallengeCtx {
    /// SHA-drift abort is in flight: do not POST a fight link.
    /// A poisoned lock is treated as closing (fail closed).
    pub(crate) fn blocks_fight_link(&self, id: &str) -> bool {
        self.closing.lock().map(|g| g.contains(id)).unwrap_or(true)
    }
}

pub struct ChallengeStart {
    pub body: String,
    pub match_id: Option<String>,
}

fn note(body: impl Into<String>) -> ChallengeStart {
    ChallengeStart {
        body: body.into(),
        match_id: None,
    }
}

pub(crate) fn silent() -> ChallengeStart {
    ChallengeStart {
        body: String::new(),
        match_id: None,
    }
}

fn already_open_note(ctx: &ChallengeCtx, id: &str) -> ChallengeStart {
    note(format!(
        "a fight is already open: {}/match/{id}",
        ctx.public_url.trim_end_matches('/')
    ))
}

async fn open_match_for_pr_retry(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<Option<db::MatchRow>, sqlx::Error> {
    match db::open_match_for_pr(pool, owner, repo, number).await {
        Ok(v) => Ok(v),
        Err(_) => db::open_match_for_pr(pool, owner, repo, number).await,
    }
}

/// Unique-collision `/fight` already proved another row. Busy lookup
/// must still comment "already open", never a generic could-not-start.
async fn already_open_now(
    ctx: &ChallengeCtx,
    owner: &str,
    repo: &str,
    number: u64,
) -> ChallengeStart {
    match open_match_for_pr_retry(&ctx.pool, owner, repo, number).await {
        Ok(Some(existing)) => already_open_note(ctx, &existing.id),
        Ok(None) | Err(_) => note("a fight is already open"),
    }
}

async fn abort_start(ctx: &ChallengeCtx, id: &str, reason: &str, body: String) -> ChallengeStart {
    match db::abort_open_retry(&ctx.pool, id, reason).await {
        Ok(true) => note(body),
        Ok(false) => silent(),
        Err(_) => {
            // Busy write is not already-closed. Keep retrying abort so
            // rematch `/fight` is not stuck on a leftover pending row.
            schedule_abort_start(ctx.clone(), id.to_string(), reason.to_string(), body);
            silent()
        }
    }
}

fn schedule_abort_start(ctx: ChallengeCtx, id: String, reason: String, body: String) {
    tokio::spawn(async move {
        for delay_ms in [25_u64, 50, 100, 200, 400, 800, 1600] {
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            match db::abort_open_match(&ctx.pool, &id, &reason).await {
                Ok(true) => {
                    ctx.start_notes.queue(id, reason, body);
                    return;
                }
                Ok(false) => return,
                Err(_) => {}
            }
        }
        ctx.start_notes.queue(id, reason, body);
    });
}

async fn abort_start_quiet(ctx: &ChallengeCtx, id: &str, reason: &str) -> ChallengeStart {
    abort_start(ctx, id, reason, "git fight could not start".into()).await
}

pub async fn start_challenge(
    ctx: &ChallengeCtx,
    installation_id: u64,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<ChallengeStart, String> {
    if !crate::gh::is_safe_github_name(owner) || !crate::gh::is_safe_github_name(repo) {
        return Ok(note("git fight could not start: invalid repository"));
    }
    let owner = crate::gh::fold_github_name(owner);
    let repo = crate::gh::fold_github_name(repo);
    match open_match_for_pr_retry(&ctx.pool, &owner, &repo, number).await {
        Ok(Some(existing)) => return Ok(already_open_note(ctx, &existing.id)),
        Ok(None) => {}
        // Busy is not proof of an open fight. Do not insert and do not
        // claim already-open; retry `/fight` or the expirer can look again.
        Err(_) => return Ok(silent()),
    }

    let recent_pr = db::count_recent_matches_for_pr(&ctx.pool, &owner, &repo, number, 3600)
        .await
        .map_err(|e| e.to_string())?;
    if recent_pr >= crate::limits::MAX_MATCHES_PER_PR_HOUR {
        return Ok(note("too many fights on this pull request; try later"));
    }

    let recent = db::count_recent_matches_for_install(&ctx.pool, installation_id, 3600)
        .await
        .map_err(|e| e.to_string())?;
    if recent >= crate::limits::MAX_MATCHES_PER_INSTALL_HOUR {
        return Ok(note("too many fights from this installation; try later"));
    }

    let repo_info = ctx.gh.get_repo(installation_id, &owner, &repo).await?;
    if repo_info.size > MAX_REPO_KB {
        return Ok(note(
            "this repo is over 1 GB, so git fight will not clone it",
        ));
    }

    let pr = match ctx
        .gh
        .poll_mergeable(installation_id, &owner, &repo, number)
        .await
    {
        Ok(pr) => pr,
        // get_pull rejects option-like SHAs at the API boundary so
        // synchronize cannot abort a live fight. /fight still owes
        // the user-visible "could not start" note.
        Err(e) if e == "pull sha" => return Ok(note("git fight could not start")),
        Err(e) => return Err(e),
    };
    match pr.mergeable {
        Some(true) => return Ok(note("no conflicts to fight")),
        None => return Ok(note("could not determine mergeability")),
        Some(false) => {}
    }
    if !gitutil::is_github_sha(&pr.head.sha)
        || !gitutil::is_github_sha(&pr.base.sha)
        || !crate::gh::is_safe_github_name(&pr.user.login)
    {
        return Ok(note("git fight could not start"));
    }

    let display_login = pr.user.login.clone();
    let Some(ours_login) = crate::gh::normalize_github_login(&display_login) else {
        return Ok(note("git fight could not start"));
    };
    let id = uuid::Uuid::new_v4().simple().to_string();
    let seed = uuid::Uuid::new_v4().as_u128() as u64;
    // Share tokens are local-demo only. GitHub matches assign roles from the session.
    match db::insert_full_match(
        &ctx.pool,
        &NewMatch {
            id: id.clone(),
            seed,
            delay: INPUT_DELAY,
            ours_name: display_login.clone(),
            theirs_name: "theirs".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: Some(ours_login.clone()),
            theirs_login: None,
            ours_token: String::new(),
            theirs_token: String::new(),
            expire_secs: ctx.expire_secs,
            installation_id: Some(installation_id as i64),
            owner: owner.clone(),
            repo: repo.clone(),
            pr_number: number as i64,
            pr_head_sha: pr.head.sha.clone(),
            pr_base_sha: pr.base.sha.clone(),
        },
    )
    .await
    {
        Ok(()) => {}
        Err(e) if db::is_unique_violation(&e) => {
            return Ok(already_open_now(ctx, &owner, &repo, number).await);
        }
        Err(e) => return Err(e.to_string()),
    }

    let work = match tempfile::Builder::new().prefix("git-fight-").tempdir() {
        Ok(w) => w,
        Err(_) => {
            return Ok(abort_start_quiet(ctx, &id, "clone").await);
        }
    };
    let dest = work.path().join("repo.git");
    let key = format!("{owner}/{repo}");
    let (url, bearer) = if let Some(local) = ctx.test_repos.get(&key) {
        (format!("file://{}", local.display()), None)
    } else {
        // HTTP. Must not `?` after insert: that would leave a pending row.
        let token = match ctx.gh.installation_token(installation_id).await {
            Ok(t) => t,
            Err(_) => return Ok(abort_start_quiet(ctx, &id, "clone").await),
        };
        (
            format!("https://github.com/{owner}/{repo}.git"),
            Some(token),
        )
    };
    // Clone/merge-tree/stats only. Token fetch and blamed-author HTTP must not
    // occupy a git worker. The whole slot hold is capped so one repo cannot
    // sit on clone then 10k cat-files.
    let prepared = {
        let _permit = match crate::limits::git_slots().acquire().await {
            Ok(p) => p,
            Err(_) => return Ok(abort_start_quiet(ctx, &id, "clone").await),
        };
        match timeout(GIT_JOB_TIMEOUT, async {
            if gitutil::clone_bare(&url, &dest, bearer.as_deref())
                .await
                .is_err()
            {
                return Err(abort_start_quiet(ctx, &id, "clone").await);
            }
            if gitutil::fetch_pr_objects(
                &dest,
                number,
                &pr.head.sha,
                &pr.base.sha,
                Some(pr.base.r#ref.as_str()),
                bearer.as_deref(),
            )
            .await
            .is_err()
            {
                return Err(abort_start_quiet(ctx, &id, "clone").await);
            }

            let (tree, paths, code) =
                match gitutil::merge_tree(&dest, &pr.base.sha, &pr.head.sha, bearer.as_deref())
                    .await
                {
                    Ok(v) => v,
                    Err(_) => {
                        return Err(abort_start_quiet(ctx, &id, "clone").await);
                    }
                };
            if code == 0 {
                return Err(
                    abort_start(ctx, &id, "no_conflicts", "no conflicts to fight".into()).await,
                );
            }

            let hunks =
                match gitutil::collect_hunks(&dest, &tree, &pr.base.sha, &paths, bearer.as_deref())
                    .await
                {
                    Ok(h) => h,
                    Err(gitutil::GitError::TooMany(n)) => {
                        return Err(abort_start(
                            ctx,
                            &id,
                            "too_many",
                            format!("too many conflicts for one fight ({n}; max {MAX_HUNKS})"),
                        )
                        .await);
                    }
                    Err(gitutil::GitError::NothingToFight) => {
                        return Err(abort_start(
                            ctx,
                            &id,
                            "nothing",
                            "the conflicts are not the kind git fight can play".into(),
                        )
                        .await);
                    }
                    Err(_) => {
                        return Err(abort_start_quiet(ctx, &id, "clone").await);
                    }
                };
            if hunks.len() > MAX_HUNKS {
                return Err(abort_start(
                    ctx,
                    &id,
                    "too_many",
                    format!(
                        "too many conflicts for one fight ({}; max {MAX_HUNKS})",
                        hunks.len()
                    ),
                )
                .await);
            }

            let mut stats = Vec::with_capacity(hunks.len());
            for h in &hunks {
                let ours_author =
                    gitutil::latest_author(&dest, &pr.head.sha, &h.path, bearer.as_deref())
                        .await
                        .unwrap_or_else(|| display_login.clone());
                let ours_stats = gitutil::fighter_stats(
                    &dest,
                    &pr.head.sha,
                    &h.path,
                    &ours_author,
                    bearer.as_deref(),
                )
                .await;
                let theirs_stats = gitutil::fighter_stats(
                    &dest,
                    &pr.base.sha,
                    &h.path,
                    &h.blame_name,
                    bearer.as_deref(),
                )
                .await;
                stats.push((ours_stats, theirs_stats));
            }
            Ok((hunks, stats))
        })
        .await
        {
            Ok(Ok(v)) => v,
            Ok(Err(start)) => return Ok(start),
            Err(_) => return Ok(abort_start_quiet(ctx, &id, "clone").await),
        }
    };

    let (mut hunks, stats) = prepared;
    for h in &mut hunks {
        h.drop_payload();
    }
    if !db::is_open_match(&ctx.pool, &id).await.unwrap_or(true) {
        return Ok(silent());
    }
    let mut login_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut sides: Vec<(String, String, Option<String>)> = Vec::new();
    for h in &hunks {
        let login = match blame_login(
            &ctx.gh,
            installation_id,
            &owner,
            &repo,
            &h.blame_sha,
            &h.blame_email,
            &mut login_cache,
        )
        .await
        {
            Ok(login) => login,
            Err(()) => return Ok(abort_start_quiet(ctx, &id, "blame").await),
        };
        sides.push(side_from_blame(&ours_login, login, &h.blame_name));
    }
    let (theirs_kind, theirs_name, theirs_login) = sides
        .first()
        .cloned()
        .unwrap_or_else(|| ("cpu".into(), "theirs".into(), None));
    let ours_kind = if theirs_kind == "mirror" {
        "mirror"
    } else {
        "github"
    };
    match db::update_match_fighters(
        &ctx.pool,
        &id,
        ours_kind,
        &theirs_kind,
        &theirs_name,
        theirs_login.as_deref(),
    )
    .await
    {
        Ok(true) => {}
        Ok(false) => return Ok(silent()),
        Err(_) => return Ok(abort_start_quiet(ctx, &id, "clone").await),
    }

    let new_hunks: Vec<db::NewHunk<'_>> = hunks
        .iter()
        .zip(stats)
        .enumerate()
        .map(|(round, (h, (ours_stats, theirs_stats)))| db::NewHunk {
            match_id: &id,
            round: round as i64,
            path: &h.path,
            hunk_index: h.hunk_index as i64,
            ours: &h.ours,
            theirs: &h.theirs,
            base: &h.base,
            theirs_login: sides.get(round).and_then(|s| s.2.as_deref()),
            theirs_name: sides.get(round).map(|s| s.1.as_str()),
            ours_stats,
            theirs_stats,
        })
        .collect();
    // Hold the fight-link slot before hunks become visible so the 5s
    // expirer cannot POST while clone finish is still posting.
    if ctx.blocks_fight_link(&id) {
        return Ok(silent());
    }
    ctx.comments.mark(&id);
    if db::insert_hunks(&ctx.pool, &new_hunks).await.is_err() {
        ctx.comments.unmark(&id);
        return Ok(abort_start_quiet(ctx, &id, "clone").await);
    }

    let rounds = hunks.len();
    // Only Ok(true) returns a fight link. Busy, already-closed, or a
    // closing latch (SHA-drift abort in flight) is silent (no match_id).
    // The 5s expirer posts uncommented open rows that still have hunks.
    if ctx.blocks_fight_link(&id) {
        ctx.comments.unmark(&id);
        return Ok(silent());
    }
    match db::is_open_match(&ctx.pool, &id).await {
        Ok(true) => {}
        Ok(false) | Err(_) => {
            ctx.comments.unmark(&id);
            return Ok(silent());
        }
    }
    Ok(ChallengeStart {
        body: fight_link_body(
            &ctx.public_url,
            &id,
            &display_login,
            &theirs_kind,
            &theirs_name,
            rounds,
        ),
        match_id: Some(id),
    })
}

pub fn is_fight_comment(body: &str) -> bool {
    body.lines()
        .next()
        .map(|l| l.trim() == "/fight")
        .unwrap_or(false)
}

pub fn is_bot_user(kind: Option<&str>, login: Option<&str>) -> bool {
    if kind == Some("Bot") {
        return true;
    }
    login.map(|l| l.ends_with("[bot]")).unwrap_or(false)
}

async fn blame_login(
    gh: &crate::gh::GitHub,
    installation_id: u64,
    owner: &str,
    repo: &str,
    sha: &str,
    email: &str,
    cache: &mut HashMap<String, Option<String>>,
) -> Result<Option<String>, ()> {
    let key = if !sha.is_empty() {
        format!("s:{sha}")
    } else {
        format!("e:{email}")
    };
    if let Some(hit) = cache.get(&key) {
        return Ok(hit.clone());
    }
    let login = match gh.login_for_commit(installation_id, owner, repo, sha).await {
        crate::gh::LoginLookup::Found(login) => Some(login),
        crate::gh::LoginLookup::Unavailable => return Err(()),
        crate::gh::LoginLookup::Rejected => None,
        crate::gh::LoginLookup::None => match gh
            .login_for_email(installation_id, owner, repo, email)
            .await
        {
            crate::gh::LoginLookup::Found(login) => Some(login),
            crate::gh::LoginLookup::Unavailable => return Err(()),
            crate::gh::LoginLookup::Rejected | crate::gh::LoginLookup::None => None,
        },
    };
    cache.insert(key, login.clone());
    if !email.is_empty() {
        cache.entry(format!("e:{email}")).or_insert(login.clone());
    }
    Ok(login)
}

pub(crate) fn fight_link_body(
    public_url: &str,
    id: &str,
    display_login: &str,
    theirs_kind: &str,
    theirs_name: &str,
    rounds: usize,
) -> String {
    let vs = vs_line(display_login, theirs_kind, theirs_name);
    let link = format!("{}/match/{id}", public_url.trim_end_matches('/'));
    format!(
        "git fight: {vs}. {rounds} round{}. {link}",
        if rounds == 1 { "" } else { "s" }
    )
}

pub(crate) fn vs_line(display_login: &str, theirs_kind: &str, theirs_name: &str) -> String {
    let ours = db::clip_comment_text(display_login);
    let theirs = db::clip_comment_text(theirs_name);
    if theirs_kind == "cpu" {
        format!("{ours} vs {theirs} (CPU)")
    } else if theirs_kind == "mirror" {
        format!("{ours} vs {ours} (mirror)")
    } else {
        format!("{ours} vs {theirs}")
    }
}

fn side_from_blame(
    ours_login: &str,
    login: Option<String>,
    blame_name: &str,
) -> (String, String, Option<String>) {
    // A login that is not a GitHub name cannot occupy a slot (OAuth
    // already dropped it). Treat that as no account: CPU under the
    // git author name, not a 24h wait for a fighter who can never join.
    let Some(raw) = login else {
        return ("cpu".into(), blame_name.to_string(), None);
    };
    let Some(stored) = crate::gh::normalize_github_login(&raw) else {
        return ("cpu".into(), blame_name.to_string(), None);
    };
    if stored.eq_ignore_ascii_case(ours_login) {
        ("mirror".into(), ours_login.to_string(), Some(stored))
    } else {
        ("github".into(), raw, Some(stored))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fight_is_first_line_only() {
        assert!(is_fight_comment("/fight\nplease"));
        assert!(is_fight_comment("  /fight  "));
        assert!(!is_fight_comment("please /fight"));
        assert!(!is_fight_comment("/fight-me"));
    }

    #[test]
    fn bots_are_ignored() {
        assert!(is_bot_user(Some("Bot"), Some("git-fight[bot]")));
        assert!(is_bot_user(Some("User"), Some("foo[bot]")));
        assert!(!is_bot_user(Some("User"), Some("alice")));
    }

    #[test]
    fn challenge_vs_line_cannot_inject_markdown() {
        assert_eq!(vs_line("alice", "cpu", "bob"), "alice vs bob (CPU)");
        assert_eq!(
            vs_line("alice", "mirror", "alice"),
            "alice vs alice (mirror)"
        );
        let vs = vs_line("alice", "cpu", "[Play](https://evil.example) @admin");
        assert!(vs.starts_with("alice vs "), "{vs}");
        assert!(vs.ends_with(" (CPU)"), "{vs}");
        assert!(!vs.contains("]("), "{vs}");
        assert!(!vs.contains('@'), "{vs}");
        assert!(!vs.contains("://"), "{vs}");
    }

    #[test]
    fn fight_link_body_matches_the_challenge_comment() {
        let body = fight_link_body("https://fight.example", "m1", "alice", "cpu", "bob", 2);
        assert_eq!(
            body,
            "git fight: alice vs bob (CPU). 2 rounds. https://fight.example/match/m1"
        );
        let one = fight_link_body("https://fight.example/", "m2", "alice", "github", "bob", 1);
        assert_eq!(
            one,
            "git fight: alice vs bob. 1 round. https://fight.example/match/m2"
        );
    }

    #[test]
    fn final_open_gate_is_silent_when_status_is_unknown() {
        assert_eq!(final_open_followup(Ok(true)), "post");
        assert_eq!(final_open_followup(Ok(false)), "silent");
        assert_eq!(
            final_open_followup(Err(())),
            "silent",
            "busy final is_open_match must not return a fight link"
        );
    }

    fn final_open_followup(open: Result<bool, ()>) -> &'static str {
        fight_link_gate_followup(false, open)
    }

    fn fight_link_gate_followup(closing: bool, open: Result<bool, ()>) -> &'static str {
        if closing {
            return "silent";
        }
        match open {
            Ok(true) => "post",
            Ok(false) | Err(()) => "silent",
        }
    }

    #[test]
    fn closing_latch_is_silent_before_a_fight_link() {
        assert_eq!(fight_link_gate_followup(false, Ok(true)), "post");
        assert_eq!(
            fight_link_gate_followup(true, Ok(true)),
            "silent",
            "SHA-drift closing must not return a match URL while abort is busy"
        );
        assert_eq!(fight_link_gate_followup(true, Err(())), "silent");
    }

    #[test]
    fn blame_login_case_is_the_same_fighter() {
        let (kind, name, login) = side_from_blame("alice", Some("Alice".into()), "Alice");
        assert_eq!(kind, "mirror");
        assert_eq!(name, "alice");
        assert_eq!(login.as_deref(), Some("alice"));
        let (kind, name, login) = side_from_blame("alice", Some("Bob".into()), "Bob");
        assert_eq!(kind, "github");
        assert_eq!(name, "Bob");
        assert_eq!(login.as_deref(), Some("bob"));
        let (kind, name, login) = side_from_blame("alice", Some("../x".into()), "Eve");
        assert_eq!(kind, "cpu");
        assert_eq!(name, "Eve");
        assert!(login.is_none());
        let (kind, name, login) = side_from_blame("alice", Some("not a login".into()), "Eve");
        assert_eq!(kind, "cpu");
        assert_eq!(name, "Eve");
        assert!(login.is_none());
    }

    fn test_ctx(pool: SqlitePool) -> ChallengeCtx {
        ChallengeCtx {
            gh: crate::gh::GitHub::new(
                "http://127.0.0.1:9".into(),
                "http://127.0.0.1:9".into(),
                1,
                String::new(),
                "id".into(),
                "sec".into(),
            ),
            pool,
            public_url: "http://fight.test".into(),
            test_repos: HashMap::new(),
            expire_secs: 3600,
            comments: CommentTrack::default(),
            start_notes: StartNoteTrack::default(),
            closing: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    #[tokio::test]
    async fn abort_start_comments_when_it_owns_the_row() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::insert_match(&pool, "m1", 1, 3, "o", "t", 3600)
            .await
            .unwrap();
        let ctx = test_ctx(pool.clone());
        let start = abort_start(&ctx, "m1", "clone", "git fight could not start".into()).await;
        assert_eq!(start.body, "git fight could not start");
        let row = crate::db::get_match(&pool, "m1").await.unwrap().unwrap();
        assert_eq!(row.status, "aborted");
        assert_eq!(row.abort_reason.as_deref(), Some("clone"));
    }

    #[tokio::test]
    async fn abort_start_is_silent_when_the_row_is_already_closed() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::insert_match(&pool, "m1", 1, 3, "o", "t", 3600)
            .await
            .unwrap();
        assert!(crate::db::abort_open_match(&pool, "m1", "outdated")
            .await
            .unwrap());
        let ctx = test_ctx(pool.clone());
        let start = abort_start(&ctx, "m1", "clone", "git fight could not start".into()).await;
        assert!(start.body.is_empty());
        assert!(start.match_id.is_none());
        let row = crate::db::get_match(&pool, "m1").await.unwrap().unwrap();
        assert_eq!(row.status, "aborted");
        assert_eq!(row.abort_reason.as_deref(), Some("outdated"));
    }

    #[test]
    fn busy_decision_abort_queues_the_comment() {
        assert_eq!(start_note_followup(Ok(true)), "comment");
        assert_eq!(start_note_followup(Ok(false)), "silent");
        assert_eq!(
            start_note_followup(Err(())),
            "queue",
            "busy abort must not drop the decision comment"
        );
    }

    fn start_note_followup(abort: Result<bool, ()>) -> &'static str {
        match abort {
            Ok(true) => "comment",
            Ok(false) => "silent",
            Err(()) => "queue",
        }
    }

    #[test]
    fn unique_collision_busy_lookup_stays_already_open() {
        assert_eq!(already_open_followup(Ok(Some("m1"))), "link");
        assert_eq!(already_open_followup(Ok(None)), "note");
        assert_eq!(
            already_open_followup(Err(())),
            "note",
            "unique collision already proved another row; busy lookup must not become could-not-start"
        );
    }

    #[test]
    fn pull_sha_reject_is_a_start_note() {
        assert_eq!(poll_err_followup("pull sha"), "note");
        assert_eq!(poll_err_followup("pull 503"), "silent");
        assert_eq!(poll_err_followup("repo 503"), "silent");
    }

    fn poll_err_followup(err: &str) -> &'static str {
        if err == "pull sha" {
            "note"
        } else {
            "silent"
        }
    }

    fn already_open_followup(lookup: Result<Option<&'static str>, ()>) -> &'static str {
        match lookup {
            Ok(Some(_)) => "link",
            Ok(None) | Err(()) => "note",
        }
    }

    #[test]
    fn busy_pre_insert_open_lookup_does_not_spawn() {
        assert_eq!(pre_insert_open_followup(Ok(Some("m1"))), "link");
        assert_eq!(pre_insert_open_followup(Ok(None)), "insert");
        assert_eq!(
            pre_insert_open_followup(Err(())),
            "silent",
            "busy open_match_for_pr must not insert or claim already-open"
        );
    }

    fn pre_insert_open_followup(lookup: Result<Option<&'static str>, ()>) -> &'static str {
        match lookup {
            Ok(Some(_)) => "link",
            Ok(None) => "insert",
            Err(()) => "silent",
        }
    }

    #[tokio::test]
    async fn already_open_now_links_the_existing_fight() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::insert_full_match(
            &pool,
            &crate::db::NewMatch {
                id: "m1".into(),
                seed: 1,
                delay: 3,
                ours_name: "alice".into(),
                theirs_name: "theirs".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: Some("alice".into()),
                theirs_login: None,
                ours_token: String::new(),
                theirs_token: String::new(),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "a".repeat(40),
                pr_base_sha: "b".repeat(40),
            },
        )
        .await
        .unwrap();
        let ctx = test_ctx(pool);
        let start = already_open_now(&ctx, "acme", "box", 1).await;
        assert!(
            start.body.contains("already open") && start.body.contains("/match/m1"),
            "{}",
            start.body
        );
    }

    #[test]
    fn abort_start_busy_write_is_not_already_closed() {
        assert!(matches!(first_or_retry(Err(()), Ok(true)), Ok(true)));
        assert!(matches!(first_or_retry(Err(()), Ok(false)), Ok(false)));
        assert!(
            first_or_retry(Err(()), Err(())).is_err(),
            "two busy aborts must retry later, not leave a pending row"
        );
    }

    fn first_or_retry(first: Result<bool, ()>, retry: Result<bool, ()>) -> Result<bool, ()> {
        first.or(retry)
    }

    #[test]
    fn persist_id_queues_until_durable() {
        assert_eq!(persist_id_followup(Ok(true), Err(())), "done");
        assert_eq!(persist_id_followup(Ok(false), Ok(Some(99))), "done");
        assert_eq!(
            persist_id_followup(Ok(false), Ok(None)),
            "queue",
            "a refused SET on a still-NULL row must not drop the posted id"
        );
        assert_eq!(persist_id_followup(Err(()), Ok(None)), "retry");
        assert_eq!(
            persist_id_followup(Err(()), Ok(Some(99))),
            "done",
            "a busy SET after another writer stored the id is done"
        );
    }

    #[tokio::test]
    async fn persist_challenge_comment_stores_id_after_abort() {
        let pool = crate::db::connect("sqlite::memory:").await.unwrap();
        crate::db::insert_match(&pool, "cmt-abort", 1, 3, "o", "t", 3600)
            .await
            .unwrap();
        assert!(db::abort_open_match(&pool, "cmt-abort", "outdated")
            .await
            .unwrap());
        let track = CommentTrack::default();
        track.mark("cmt-abort");
        persist_challenge_comment(&pool, &track, "cmt-abort", Ok(88)).await;
        assert_eq!(
            db::get_match(&pool, "cmt-abort")
                .await
                .unwrap()
                .unwrap()
                .challenge_comment_id,
            Some(88)
        );
        assert!(!track.is_inflight("cmt-abort"));
        assert!(!track.has_pending("cmt-abort"));
    }

    #[test]
    fn missing_posted_id_holds_inflight() {
        assert_eq!(persist_posted_followup(Ok(7)), PersistPosted::Set(7));
        assert_eq!(persist_posted_followup(Ok(0)), PersistPosted::Hold);
        assert_eq!(
            persist_posted_followup(Err("comment id")),
            PersistPosted::Hold,
            "a 2xx body without an id must not POST a second fight link"
        );
        assert_eq!(
            persist_posted_followup(Err("comment 502")),
            PersistPosted::Retry,
            "a failed POST must unmark so the expirer can try again"
        );
    }

    #[test]
    fn comment_track_skips_inflight_and_pending() {
        let track = CommentTrack::default();
        track.mark("m1");
        assert!(track.is_inflight("m1"));
        assert!(!track.has_pending("m1"));
        track.queue_id("m1".into(), 42);
        track.unmark("m1");
        assert!(!track.is_inflight("m1"));
        assert!(track.has_pending("m1"));
        assert_eq!(track.pending_snapshot(), vec![("m1".into(), 42)]);
        track.dequeue_id("m1");
        assert!(!track.has_pending("m1"));
    }
}
