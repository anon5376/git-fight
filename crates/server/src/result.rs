//! After the last round: resolve files, plumbing commit, create-only push.

use crate::db::{self, HunkRow, MatchRow};
use crate::gh::GitHub;
use crate::gitutil;
use git_fight_core::{ConflictFile, Pick};
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone)]
pub struct ResultCtx {
    pub gh: Option<GitHub>,
    pub pool: SqlitePool,
    pub public_url: String,
    pub test_repos: std::collections::HashMap<String, PathBuf>,
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
pub fn git_pick_for_winner(winner: &str) -> Option<Pick> {
    match winner {
        "ours" | "forfeit_theirs" => Some(Pick::Theirs),
        "theirs" | "forfeit_ours" => Some(Pick::Ours),
        _ => None,
    }
}

fn unresolved_paths(hunks: &[HunkRow]) -> Vec<String> {
    hunks
        .iter()
        .filter(|h| git_pick_for_winner(h.winner.as_deref().unwrap_or("")).is_none())
        .map(|h| format!("{} hunk {}", h.path, h.hunk_index))
        .collect()
}

fn round_lines(hunks: &[HunkRow]) -> String {
    hunks
        .iter()
        .map(|h| {
            format!(
                "round {}: {} hunk {} {}",
                h.round_index + 1,
                h.path,
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
    let hunks = db::list_hunks(&ctx.pool, match_id)
        .await
        .map_err(|e| e.to_string())?;
    if hunks.is_empty() {
        return Ok(());
    }

    let unresolved = unresolved_paths(&hunks);
    if !unresolved.is_empty() {
        let body = format!(
            "git fight: nothing pushed — unresolved conflicts (draw):\n{}\nComment `/fight` to try again.\nreplay: {}/replay/{match_id}",
            unresolved.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n"),
            ctx.public_url.trim_end_matches('/'),
        );
        comment(ctx, &row, &body).await;
        let _ = db::set_result_branch(&ctx.pool, match_id, None, Some("draw")).await;
        return Ok(());
    }

    if let Some(gh) = &ctx.gh {
        if let Some(inst) = row.installation_id.map(|i| i as u64) {
            let pr = gh
                .get_pull(inst, &row.owner, &row.repo, row.pr_number as u64)
                .await?;
            if !pr.head.sha.eq_ignore_ascii_case(&row.pr_head_sha)
                || !pr.base.sha.eq_ignore_ascii_case(&row.pr_base_sha)
            {
                let body = format!(
                    "git fight: this fight used outdated code (PR head or base moved). Nothing was pushed. Comment `/fight` for a rematch.\nreplay: {}/replay/{match_id}",
                    ctx.public_url.trim_end_matches('/'),
                );
                comment(ctx, &row, &body).await;
                let _ = db::set_result_branch(&ctx.pool, match_id, None, Some("outdated")).await;
                return Ok(());
            }
        }
    }

    let branch = gitutil::result_ref(row.pr_number, &row.id).map_err(|e| e.to_string())?;
    let work = tempfile::Builder::new()
        .prefix("git-fight-result-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let dest = work.path().join("repo.git");
    let key = format!("{}/{}", row.owner, row.repo);
    let (url, bearer) = if let Some(local) = ctx.test_repos.get(&key) {
        (format!("file://{}", local.display()), None)
    } else {
        let inst = row
            .installation_id
            .ok_or_else(|| "missing installation".to_string())? as u64;
        let gh = ctx
            .gh
            .as_ref()
            .ok_or_else(|| "missing github".to_string())?;
        let token = gh.installation_token(inst).await?;
        (
            format!("https://github.com/{}/{}", row.owner, row.repo) + ".git",
            Some(token),
        )
    };
    gitutil::clone_bare(&url, &dest, bearer.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let _ = gitutil::fetch_shas(
        &dest,
        &[&row.pr_head_sha, &row.pr_base_sha],
        bearer.as_deref(),
    )
    .await;

    let (tree, paths, code) = gitutil::merge_tree(&dest, &row.pr_base_sha, &row.pr_head_sha)
        .await
        .map_err(|e| e.to_string())?;
    if code == 0 {
        return Err("merge became clean before push".into());
    }

    let picks_by_path = grouped_picks(&hunks);
    let mut files = Vec::new();
    for path in &paths {
        let Some(picks) = picks_by_path.get(path) else {
            continue;
        };
        if !gitutil::is_safe_path(path) {
            continue;
        }
        let blob = gitutil::cat_blob(&dest, &format!("{tree}:{path}"))
            .await
            .map_err(|e| e.to_string())?;
        let parsed = ConflictFile::parse(&blob).map_err(|e| e.to_string())?;
        files.push((path.clone(), parsed.resolve(picks)));
    }

    let new_tree = gitutil::build_resolved_tree(&dest, &tree, &files)
        .await
        .map_err(|e| e.to_string())?;
    let message = format!("git fight match {}\n\n{}\n", row.id, round_lines(&hunks));
    let commit = gitutil::commit_tree(
        &dest,
        &new_tree,
        &[&row.pr_head_sha, &row.pr_base_sha],
        &message,
    )
    .await
    .map_err(|e| e.to_string())?;
    gitutil::push_create_only(&dest, &url, &commit, &branch, bearer.as_deref())
        .await
        .map_err(|e| e.to_string())?;

    let _ = db::set_result_branch(&ctx.pool, match_id, Some(&branch), None).await;
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
    let public = ctx.public_url.trim_end_matches('/');
    let body = format!(
        "git fight: this match expired before anyone finished. Nothing was pushed.\nopen match: {public}/match/{}",
        row.id
    );
    comment(ctx, row, &body).await;
}
