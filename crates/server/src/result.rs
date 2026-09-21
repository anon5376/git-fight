//! After the last round: resolve files, plumbing commit, create-only push.

use crate::db::{self, clip_display_path, HunkRow, MatchRow};
use crate::gh::GitHub;
use crate::gitutil::{self, ExistingResult};
use crate::limits::GIT_JOB_TIMEOUT;
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

fn unresolved_paths(hunks: &[HunkRow]) -> Vec<String> {
    hunks
        .iter()
        .filter(|h| git_pick_for_winner(h.winner.as_deref().unwrap_or("")).is_none())
        .map(|h| {
            format!(
                "{} hunk {} ({})",
                clip_display_path(&h.path),
                h.hunk_index,
                h.winner.as_deref().unwrap_or("unresolved")
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
                h.round_index + 1,
                clip_display_path(&h.path),
                h.hunk_index,
                h.winner.as_deref().unwrap_or("unresolved")
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
                Err(_) => {
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
            };
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
        Err(_) => {
            return skip_push(
                ctx,
                &row,
                match_id,
                "push",
                format!(
                    "git fight: nothing pushed — could not name the result branch. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/')
                ),
            )
            .await;
        }
    };
    let work = match tempfile::Builder::new()
        .prefix("git-fight-result-")
        .tempdir()
    {
        Ok(w) => w,
        Err(_) => {
            return skip_clone(ctx, &row, match_id).await;
        }
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
                "clone",
                format!(
                    "git fight: nothing pushed — invalid repository name. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/')
                ),
            )
            .await;
        }
        let Some(inst) = row.installation_id.map(|i| i as u64) else {
            return skip_clone(ctx, &row, match_id).await;
        };
        let Some(gh) = ctx.gh.as_ref() else {
            return skip_clone(ctx, &row, match_id).await;
        };
        let token = match gh.installation_token(inst).await {
            Ok(t) => t,
            Err(_) => return skip_clone(ctx, &row, match_id).await,
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
            Err(_) => return skip_clone(ctx, &row, match_id).await,
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
            if db::set_result_branch(&ctx.pool, match_id, Some(&branch), None)
                .await
                .unwrap_or(false)
            {
                let public = ctx.public_url.trim_end_matches('/');
                let compare = format!(
                    "https://github.com/{}/{}/compare/{}...{}",
                    row.owner, row.repo, row.pr_head_sha, branch
                );
                let body = format!(
                    "git fight finished.\n{}\nbranch: `{branch}`\ncompare: {compare}\nreplay: {public}/replay/{match_id}",
                    round_lines(&hunks)
                );
                comment(ctx, &row, &body).await;
            }
            Ok(())
        }
        Err((reason, body)) => skip_push(ctx, &row, match_id, reason, body).await,
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

async fn skip_clone(ctx: &ResultCtx, row: &MatchRow, match_id: &str) -> Result<(), String> {
    skip_push(
        ctx,
        row,
        match_id,
        "clone",
        format!(
            "git fight: nothing pushed — could not clone to write the result branch. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
            ctx.public_url.trim_end_matches('/')
        ),
    )
    .await
}

async fn skip_push(
    ctx: &ResultCtx,
    row: &MatchRow,
    match_id: &str,
    reason: &str,
    body: String,
) -> Result<(), String> {
    if db::set_result_branch(&ctx.pool, match_id, None, Some(reason))
        .await
        .unwrap_or(false)
    {
        comment(ctx, row, &body).await;
    }
    Ok(())
}

fn grouped_picks(hunks: &[HunkRow]) -> BTreeMap<String, Vec<Option<Pick>>> {
    let mut max_idx: BTreeMap<String, usize> = BTreeMap::new();
    for h in hunks {
        let n = h.hunk_index as usize;
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
        if let Some(slot) = out
            .get_mut(&h.path)
            .and_then(|v| v.get_mut(h.hunk_index as usize))
        {
            *slot = git_pick_for_winner(h.winner.as_deref().unwrap_or(""));
        }
    }
    out
}

async fn comment(ctx: &ResultCtx, row: &MatchRow, body: &str) {
    let Some(gh) = &ctx.gh else {
        return;
    };
    let Some(inst) = row.installation_id.map(|i| i as u64) else {
        return;
    };
    if row.pr_number <= 0 || row.owner.is_empty() {
        return;
    }
    if !crate::gh::is_safe_github_name(&row.owner) || !crate::gh::is_safe_github_name(&row.repo) {
        return;
    }
    let _ = gh
        .issue_comment(
            inst,
            &row.owner,
            &row.repo,
            row.pr_number as u64,
            row.challenge_comment_id,
            body,
        )
        .await;
}

pub(crate) async fn comment_expired(ctx: &ResultCtx, row: &MatchRow) {
    let body = "git fight: this match expired before anyone finished. Nothing was pushed. Comment `/fight` for a rematch.".to_string();
    comment(ctx, row, &body).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comment_paths_are_length_capped() {
        let long = format!("{}/lib.rs", "dir/".repeat(80));
        let clipped = clip_display_path(&long);
        assert!(clipped.chars().count() <= 160);
        assert!(clipped.ends_with('…'), "{clipped}");
        assert_eq!(clip_display_path("lib.rs"), "lib.rs");
    }
}
