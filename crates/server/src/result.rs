//! After the last round: resolve files, plumbing commit, create-only push.

use crate::db::{self, clip_comment_text, HunkRow, MatchRow};
use crate::gh::GitHub;
use crate::gitutil::{self, ExistingResult};
use crate::limits::{GIT_JOB_TIMEOUT, MAX_HUNKS};
use git_fight_core::{ConflictFile, Pick};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::timeout;

#[derive(Clone)]
pub struct ResultCtx {
    pub gh: Option<GitHub>,
    pub pool: SqlitePool,
    pub public_url: String,
    pub test_repos: HashMap<String, PathBuf>,
    /// One in-flight publish per match so boot retry and the room task cannot
    /// both comment a skip after a successful create-only push.
    pub publishing: Arc<Mutex<HashSet<String>>>,
    /// 24h expiry comments that failed HTTP. Restart loses this; a later
    /// `/fight` still works because the row is already expired.
    pub pending_expired: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl ResultCtx {
    pub(crate) fn queue_expired(&self, id: &str) {
        if let Ok(mut g) = self.pending_expired.lock() {
            g.insert(id.to_string());
        }
    }

    fn dequeue_expired(&self, id: &str) {
        if let Ok(mut g) = self.pending_expired.lock() {
            g.remove(id);
        }
    }

    pub(crate) fn expired_snapshot(&self) -> Vec<String> {
        self.pending_expired
            .lock()
            .map(|g| g.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl ResultCtx {
    pub fn spawn_publish(&self, id: impl Into<String>) {
        let id = id.into();
        let ctx = self.clone();
        tokio::spawn(async move {
            ctx.publish_claimed(&id).await;
        });
    }

    pub async fn publish_claimed(&self, id: &str) {
        {
            let mut g = self.publishing.lock().await;
            if !g.insert(id.to_string()) {
                return;
            }
        }
        if publish(self, id).await.is_err() {
            eprintln!("git fight result failed");
        }
        self.publishing.lock().await.remove(id);
    }

    pub(crate) async fn retry_pending_expired(&self) {
        for id in self.expired_snapshot() {
            let Ok(Some(row)) = db::get_match(&self.pool, &id).await else {
                self.dequeue_expired(&id);
                continue;
            };
            if row.status != "expired" {
                self.dequeue_expired(&id);
                continue;
            }
            if comment_expired(self, &row).await.is_ok() {
                self.dequeue_expired(&id);
            }
        }
    }
}

pub fn winner_tag(result: git_fight_core::RoundResult, forfeit: bool) -> &'static str {
    match result {
        git_fight_core::RoundResult::Ours => {
            if forfeit {
                "forfeit_theirs"
            } else {
                "ours"
            }
        }
        git_fight_core::RoundResult::Theirs => {
            if forfeit {
                "forfeit_ours"
            } else {
                "theirs"
            }
        }
        git_fight_core::RoundResult::Draw => "draw",
    }
}

/// Game Ours is the PR (git-theirs in merge-tree). Game Theirs is base (git-ours).
/// Forfeit and draw leave the hunk unresolved: they are not a side pick.
pub fn git_pick_for_winner(winner: &str) -> Option<Pick> {
    match winner {
        "ours" => Some(Pick::Theirs),
        "theirs" => Some(Pick::Ours),
        _ => None,
    }
}

fn comment_winner(winner: Option<&str>) -> &'static str {
    match winner {
        Some("ours") => "ours",
        Some("theirs") => "theirs",
        Some("draw") => "draw",
        Some("forfeit_ours") => "forfeit_ours",
        Some("forfeit_theirs") => "forfeit_theirs",
        _ => "unresolved",
    }
}

fn comment_round_number(round: i64) -> i64 {
    if (0..MAX_HUNKS as i64).contains(&round) {
        round + 1
    } else {
        0
    }
}

fn comment_hunk_index(index: i64) -> i64 {
    if (0..MAX_HUNKS as i64).contains(&index) {
        index
    } else {
        0
    }
}

fn unresolved_paths(hunks: &[HunkRow]) -> Vec<String> {
    hunks
        .iter()
        .filter(|h| git_pick_for_winner(h.winner.as_deref().unwrap_or("")).is_none())
        .map(|h| {
            format!(
                "{} hunk {} ({})",
                clip_comment_text(&h.path),
                comment_hunk_index(h.hunk_index),
                comment_winner(h.winner.as_deref())
            )
        })
        .collect()
}

fn skip_reason(hunks: &[HunkRow]) -> &'static str {
    if hunks
        .iter()
        .any(|h| matches!(h.winner.as_deref(), Some("forfeit_ours" | "forfeit_theirs")))
    {
        "forfeit"
    } else {
        "draw"
    }
}

fn round_lines(hunks: &[HunkRow]) -> String {
    hunks
        .iter()
        .map(|h| {
            format!(
                "round {}: {} hunk {} {}",
                comment_round_number(h.round_index),
                clip_comment_text(&h.path),
                comment_hunk_index(h.hunk_index),
                comment_winner(h.winner.as_deref())
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn publish(ctx: &ResultCtx, match_id: &str) -> Result<(), String> {
    let row = db::get_match(&ctx.pool, match_id)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "match missing".to_string())?;
    if row.pr_number <= 0 || row.owner.is_empty() {
        return Ok(());
    }
    if row.result_branch.is_some() {
        return Ok(());
    }
    if matches!(row.status.as_str(), "aborted" | "expired") || row.abort_reason.is_some() {
        return Ok(());
    }
    let hunks = db::list_hunks(&ctx.pool, match_id)
        .await
        .map_err(|e| e.to_string())?;
    if hunks.is_empty() {
        return Ok(());
    }

    let unresolved = unresolved_paths(&hunks);
    if !unresolved.is_empty() {
        let reason = skip_reason(&hunks);
        let body = format!(
            "git fight: nothing pushed — unresolved conflicts ({reason}):\n{}\nComment `/fight` to try again.\nreplay: {}/replay/{match_id}",
            unresolved.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"),
            ctx.public_url.trim_end_matches('/'),
        );
        return skip_push(ctx, &row, match_id, reason, body).await;
    }

    if !crate::gh::is_safe_github_name(&row.owner)
        || !crate::gh::is_safe_github_name(&row.repo)
        || !gitutil::is_github_sha(&row.pr_head_sha)
        || !gitutil::is_github_sha(&row.pr_base_sha)
    {
        return skip_push(
            ctx,
            &row,
            match_id,
            "recheck",
            format!(
                "git fight: nothing pushed — could not re-check the pull request. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                ctx.public_url.trim_end_matches('/')
            ),
        )
        .await;
    }

    let mut base_ref = String::new();
    if let Some(gh) = &ctx.gh {
        if let Some(inst) = row.installation_id.map(|i| i as u64) {
            let pr = match gh
                .get_pull(inst, &row.owner, &row.repo, row.pr_number as u64)
                .await
            {
                Ok(pr) => pr,
                Err(e) => {
                    // Gone PR cannot grow a result. A 5xx/timeout must stay
                    // unpublished so boot and the expirer retry the push.
                    if e.contains("pull 404") || e.contains("pull 410") {
                        return skip_push(
                            ctx,
                            &row,
                            match_id,
                            "recheck",
                            format!(
                                "git fight: nothing pushed — could not re-check the pull request. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                                ctx.public_url.trim_end_matches('/')
                            ),
                        )
                        .await;
                    }
                    return retry_later("pull");
                }
            };
            if !gitutil::is_github_sha(&pr.head.sha) || !gitutil::is_github_sha(&pr.base.sha) {
                return retry_later("pull");
            }
            if !pr.head.sha.eq_ignore_ascii_case(&row.pr_head_sha)
                || !pr.base.sha.eq_ignore_ascii_case(&row.pr_base_sha)
            {
                let body = format!(
                    "git fight: this fight used outdated code (PR head or base moved). Nothing was pushed. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/'),
                );
                return skip_push(ctx, &row, match_id, "outdated", body).await;
            }
            base_ref = pr.base.r#ref;
        }
    }

    let branch = match gitutil::result_ref(row.pr_number, &row.id) {
        Ok(b) => b,
        Err(_) => return retry_later("push"),
    };
    let work = match tempfile::Builder::new()
        .prefix("git-fight-result-")
        .tempdir()
    {
        Ok(w) => w,
        Err(_) => return retry_later("clone"),
    };
    let dest = work.path().join("repo.git");
    let key = format!("{}/{}", row.owner, row.repo);
    let (url, bearer) = if let Some(local) = ctx.test_repos.get(&key) {
        (format!("file://{}", local.display()), None)
    } else {
        if !crate::gh::is_safe_github_name(&row.owner) || !crate::gh::is_safe_github_name(&row.repo)
        {
            return skip_push(
                ctx,
                &row,
                match_id,
                "recheck",
                format!(
                    "git fight: nothing pushed — invalid repository name. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/')
                ),
            )
            .await;
        }
        let Some(inst) = row.installation_id.map(|i| i as u64) else {
            return retry_later("clone");
        };
        let Some(gh) = ctx.gh.as_ref() else {
            return retry_later("clone");
        };
        let token = match gh.installation_token(inst).await {
            Ok(t) => t,
            Err(_) => return retry_later("clone"),
        };
        (
            format!("https://github.com/{}/{}.git", row.owner, row.repo),
            Some(token),
        )
    };
    // Clone/merge-tree/push only. Comments must not take a slot. Whole hold is capped.
    let git = {
        let _permit = match crate::limits::git_slots().acquire().await {
            Ok(p) => p,
            Err(_) => return retry_later("clone"),
        };
        match timeout(
            GIT_JOB_TIMEOUT,
            push_result_git(
                &dest,
                &url,
                bearer.as_deref(),
                &row,
                match_id,
                &branch,
                &hunks,
                &base_ref,
                &ctx.public_url,
            ),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => Err((
                "clone",
                format!(
                    "git fight: nothing pushed — could not clone to write the result branch. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/')
                ),
            )),
        }
    };
    match git {
        Ok(()) => {
            let public = ctx.public_url.trim_end_matches('/');
            let compare = format!(
                "https://github.com/{}/{}/compare/{}...{}",
                row.owner, row.repo, row.pr_head_sha, branch
            );
            let body = format!(
                "git fight finished.\n{}\nbranch: `{branch}`\ncompare: {compare}\nreplay: {public}/replay/{match_id}",
                round_lines(&hunks)
            );
            comment(ctx, &row, &body).await?;
            db::set_result_branch(&ctx.pool, match_id, Some(&branch), None)
                .await
                .map_err(|e| e.to_string())?;
            Ok(())
        }
        Err((reason, body)) => {
            if is_decision_skip(reason) {
                skip_push(ctx, &row, match_id, reason, body).await
            } else {
                retry_later(reason)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn push_result_git(
    dest: &Path,
    url: &str,
    bearer: Option<&str>,
    row: &MatchRow,
    match_id: &str,
    branch: &str,
    hunks: &[HunkRow],
    base_ref: &str,
    public_url: &str,
) -> Result<(), (&'static str, String)> {
    let public = public_url.trim_end_matches('/');
    if gitutil::clone_bare(url, dest, bearer).await.is_err() {
        return Err((
            "clone",
            format!(
                "git fight: nothing pushed — could not clone to write the result branch. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
            ),
        ));
    }
    match gitutil::inspect_result_ref(
        dest,
        url,
        branch,
        match_id,
        &row.pr_head_sha,
        &row.pr_base_sha,
        bearer,
    )
    .await
    {
        Ok(ExistingResult::Ours) => return Ok(()),
        Ok(ExistingResult::Foreign) => {
            return Err((
                "exists",
                format!(
                    "git fight: nothing pushed — `{branch}` already exists. The bot never overwrites a branch. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                ),
            ));
        }
        Ok(ExistingResult::Missing) | Err(_) => {}
    }
    if gitutil::fetch_pr_objects(
        dest,
        row.pr_number as u64,
        &row.pr_head_sha,
        &row.pr_base_sha,
        if base_ref.is_empty() {
            None
        } else {
            Some(base_ref)
        },
        bearer,
    )
    .await
    .is_err()
    {
        return Err((
            "push",
            format!(
                "git fight: nothing pushed — could not rebuild the merge. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
            ),
        ));
    }

    let (tree, paths, code) = match gitutil::merge_tree(
        dest,
        &row.pr_base_sha,
        &row.pr_head_sha,
        bearer,
    )
    .await
    {
        Ok(v) => v,
        Err(_) => {
            return Err((
                "push",
                format!(
                    "git fight: nothing pushed — could not rebuild the merge. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                ),
            ));
        }
    };
    if code == 0 {
        return Err((
            "no_conflicts",
            format!(
                "git fight: nothing pushed — the pull request became mergeable. Comment `/fight` if conflicts return.\nreplay: {public}/replay/{match_id}"
            ),
        ));
    }

    let picks_by_path = grouped_picks(hunks);
    let mut files = Vec::new();
    for path in &paths {
        let Some(picks) = picks_by_path.get(path) else {
            continue;
        };
        if !gitutil::is_safe_path(path) {
            continue;
        }
        let blob = match gitutil::cat_blob(dest, &format!("{tree}:{path}"), bearer).await {
            Ok(b) => b,
            Err(_) => {
                return Err((
                    "push",
                    format!(
                        "git fight: nothing pushed — could not read a conflicted file. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                    ),
                ));
            }
        };
        let parsed = match ConflictFile::parse(&blob) {
            Ok(p) => p,
            Err(_) => {
                return Err((
                    "push",
                    format!(
                        "git fight: nothing pushed — could not parse a conflicted file. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                    ),
                ));
            }
        };
        files.push((path.clone(), parsed.resolve(picks)));
    }

    let new_tree = match gitutil::build_resolved_tree(dest, &tree, &files, bearer).await {
        Ok(t) => t,
        Err(_) => {
            return Err((
                "push",
                format!(
                    "git fight: nothing pushed — could not write the result tree. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                ),
            ));
        }
    };
    let message = format!("git fight match {}\n\n{}\n", row.id, round_lines(hunks));
    let commit = match gitutil::commit_tree(
        dest,
        &new_tree,
        &[&row.pr_head_sha, &row.pr_base_sha],
        &message,
        bearer,
    )
    .await
    {
        Ok(c) => c,
        Err(_) => {
            return Err((
                "push",
                format!(
                    "git fight: nothing pushed — could not write the result commit. Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
                ),
            ));
        }
    };
    if let Err(e) = gitutil::push_create_only(dest, url, &commit, branch, bearer).await {
        let exists = e.to_string().contains("already exists");
        let reason = if exists { "exists" } else { "push" };
        let why = if exists {
            format!("`{branch}` already exists. The bot never overwrites a branch.")
        } else {
            format!("could not create `{branch}`.")
        };
        return Err((
            reason,
            format!(
                "git fight: nothing pushed — {why} Comment `/fight` for a rematch.\nreplay: {public}/replay/{match_id}"
            ),
        ));
    }
    Ok(())
}

/// Draw / forfeit / outdated / exists / gone PR. Not clone, token, or push I/O.
fn is_decision_skip(reason: &str) -> bool {
    matches!(
        reason,
        "draw"
            | "forfeit"
            | "outdated"
            | "exists"
            | "no_conflicts"
            | "expired"
            | "too_many"
            | "recheck"
    )
}

fn retry_later(why: &str) -> Result<(), String> {
    Err(why.to_string())
}

async fn skip_push(
    ctx: &ResultCtx,
    row: &MatchRow,
    match_id: &str,
    reason: &str,
    body: String,
) -> Result<(), String> {
    comment(ctx, row, &body).await?;
    db::set_result_branch(&ctx.pool, match_id, None, Some(reason))
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn hunk_slot(index: i64) -> Option<usize> {
    let n = usize::try_from(index).ok()?;
    (n < MAX_HUNKS).then_some(n)
}

fn grouped_picks(hunks: &[HunkRow]) -> BTreeMap<String, Vec<Option<Pick>>> {
    let mut max_idx: BTreeMap<String, usize> = BTreeMap::new();
    for h in hunks {
        let Some(n) = hunk_slot(h.hunk_index) else {
            continue;
        };
        max_idx
            .entry(h.path.clone())
            .and_modify(|m| *m = (*m).max(n))
            .or_insert(n);
    }
    let mut out: BTreeMap<String, Vec<Option<Pick>>> = BTreeMap::new();
    for (path, max) in max_idx {
        out.insert(path, vec![None; max + 1]);
    }
    for h in hunks {
        let Some(n) = hunk_slot(h.hunk_index) else {
            continue;
        };
        if let Some(slot) = out.get_mut(&h.path).and_then(|v| v.get_mut(n)) {
            *slot = git_pick_for_winner(h.winner.as_deref().unwrap_or(""));
        }
    }
    out
}

async fn comment(ctx: &ResultCtx, row: &MatchRow, body: &str) -> Result<(), String> {
    let Some(gh) = &ctx.gh else {
        return Ok(());
    };
    let Some(inst) = row.installation_id.map(|i| i as u64) else {
        return Ok(());
    };
    if row.pr_number <= 0 || row.owner.is_empty() {
        return Ok(());
    }
    if !crate::gh::is_safe_github_name(&row.owner) || !crate::gh::is_safe_github_name(&row.repo) {
        return Ok(());
    }
    gh.issue_comment(
        inst,
        &row.owner,
        &row.repo,
        row.pr_number as u64,
        row.challenge_comment_id,
        body,
    )
    .await
    .map(|_| ())
}

pub(crate) async fn comment_expired(ctx: &ResultCtx, row: &MatchRow) -> Result<(), String> {
    let body = "git fight: this match expired before anyone finished. Nothing was pushed. Comment `/fight` for a rematch.".to_string();
    comment(ctx, row, &body).await
}

pub(crate) async fn comment_decision(
    ctx: &ResultCtx,
    row: &MatchRow,
    body: &str,
) -> Result<(), String> {
    comment(ctx, row, body).await
}

pub(crate) async fn comment_outdated(ctx: &ResultCtx, row: &MatchRow) -> Result<(), String> {
    let public = ctx.public_url.trim_end_matches('/');
    let body = format!(
        "git fight: this fight used outdated code (PR head or base moved). Nothing will be pushed. Comment `/fight` for a rematch.\nopen match: {public}/match/{}",
        row.id
    );
    comment(ctx, row, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_is_recorded_only_after_comment() {
        assert_eq!(outcome_record_followup(Ok(())), "set");
        assert_eq!(
            outcome_record_followup(Err(())),
            "retry",
            "a failed outcome comment must stay unpublished"
        );
    }

    #[test]
    fn expired_comment_failure_is_retried() {
        assert_eq!(expired_comment_followup(Ok(())), "done");
        assert_eq!(
            expired_comment_followup(Err(())),
            "queue",
            "a failed expiry PATCH must not go silent"
        );
    }

    fn expired_comment_followup(comment: Result<(), ()>) -> &'static str {
        match comment {
            Ok(()) => "done",
            Err(()) => "queue",
        }
    }

    fn outcome_record_followup(comment: Result<(), ()>) -> &'static str {
        match comment {
            Ok(()) => "set",
            Err(()) => "retry",
        }
    }

    #[test]
    fn comment_paths_are_length_capped() {
        let long = format!("{}/lib.rs", "dir/".repeat(80));
        let clipped = db::clip_display_path(&long);
        assert!(clipped.chars().count() <= 160);
        assert!(clipped.ends_with('…'), "{clipped}");
        assert_eq!(db::clip_display_path("lib.rs"), "lib.rs");
    }

    #[test]
    fn result_comments_cannot_inject_markdown() {
        let row = |path: &str, winner: Option<&str>, round: i64, index: i64| HunkRow {
            round_index: round,
            path: path.into(),
            hunk_index: index,
            winner: winner.map(str::to_string),
            theirs_name: None,
            theirs_login: None,
            ours_hp: 100,
            ours_armor: false,
            ours_special: false,
            theirs_hp: 100,
            theirs_armor: false,
            theirs_special: false,
            is_ko: false,
        };
        let lines = round_lines(&[row(
            "[click](https://evil.example/phish).rs",
            Some("ours"),
            0,
            0,
        )]);
        assert!(lines.contains("round 1:"), "{lines}");
        assert!(lines.ends_with(" ours"), "{lines}");
        assert!(!lines.contains("]("), "{lines}");
        assert!(!lines.contains("://"), "{lines}");
        let skip = unresolved_paths(&[row(
            "![img](https://evil.example/x.png)",
            Some("[x](https://evil.example)"),
            0,
            0,
        )]);
        assert_eq!(skip.len(), 1);
        assert!(!skip[0].contains("]("), "{}", skip[0]);
        assert!(!skip[0].contains("://"), "{}", skip[0]);
        assert!(skip[0].contains("unresolved"), "{}", skip[0]);
        assert_eq!(
            round_lines(&[row("lib.rs", Some("ours"), i64::MAX, i64::MAX)]),
            "round 0: lib.rs hunk 0 ours"
        );
    }

    #[test]
    fn grouped_picks_ignores_hostile_hunk_index() {
        let row = |index: i64| HunkRow {
            round_index: 0,
            path: "lib.rs".into(),
            hunk_index: index,
            winner: Some("ours".into()),
            theirs_name: None,
            theirs_login: None,
            ours_hp: 100,
            ours_armor: false,
            ours_special: false,
            theirs_hp: 100,
            theirs_armor: false,
            theirs_special: false,
            is_ko: false,
        };
        assert!(grouped_picks(&[row(i64::MAX), row(-1)]).is_empty());
        let picks = grouped_picks(&[row(0)]);
        assert_eq!(picks.get("lib.rs").map(Vec::len), Some(1));
        assert_eq!(picks["lib.rs"][0], Some(Pick::Theirs));
    }

    #[test]
    fn only_decision_skips_are_permanent() {
        for reason in [
            "draw",
            "forfeit",
            "outdated",
            "exists",
            "no_conflicts",
            "expired",
            "too_many",
            "recheck",
        ] {
            assert!(is_decision_skip(reason), "{reason}");
        }
        for reason in ["clone", "push", "pull", "token"] {
            assert!(!is_decision_skip(reason), "{reason}");
        }
    }
}
