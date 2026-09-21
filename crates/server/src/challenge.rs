//! Start a match from a conflicted pull request.

use crate::db::{self, NewMatch};
use crate::gh::GitHub;
use crate::gitutil;
use crate::limits::{MAX_HUNKS, MAX_REPO_KB};
use crate::protocol::INPUT_DELAY;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;

pub struct ChallengeCtx {
    pub gh: GitHub,
    pub pool: SqlitePool,
    pub public_url: String,
    pub test_repos: HashMap<String, PathBuf>,
    pub expire_secs: i64,
}

pub async fn start_challenge(
    ctx: &ChallengeCtx,
    installation_id: u64,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<String, String> {
    if let Some(existing) = db::open_match_for_pr(&ctx.pool, owner, repo, number)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(format!(
            "a fight is already open: {}/match/{}",
            ctx.public_url.trim_end_matches('/'),
            existing.id
        ));
    }

    let recent_pr = db::count_recent_matches_for_pr(&ctx.pool, owner, repo, number, 3600)
        .await
        .map_err(|e| e.to_string())?;
    if recent_pr >= crate::limits::MAX_MATCHES_PER_PR_HOUR {
        return Ok("too many fights on this pull request; try later".into());
    }

    let recent = db::count_recent_matches_for_install(&ctx.pool, installation_id, 3600)
        .await
        .map_err(|e| e.to_string())?;
    if recent >= crate::limits::MAX_MATCHES_PER_INSTALL_HOUR {
        return Ok("too many fights from this installation; try later".into());
    }

    let repo_info = ctx.gh.get_repo(installation_id, owner, repo).await?;
    if repo_info.size > MAX_REPO_KB {
        return Ok("this repo is over 1 GB, so git fight will not clone it".into());
    }

    let pr = ctx
        .gh
        .poll_mergeable(installation_id, owner, repo, number)
        .await?;
    match pr.mergeable {
        Some(true) => return Ok("no conflicts to fight".into()),
        None => return Ok("could not determine mergeability".into()),
        Some(false) => {}
    }

    let work = tempfile::Builder::new()
        .prefix("git-fight-")
        .tempdir()
        .map_err(|e| e.to_string())?;
    let dest = work.path().join("repo.git");
    let key = format!("{owner}/{repo}");
    let (url, bearer) = if let Some(local) = ctx.test_repos.get(&key) {
        (format!("file://{}", local.display()), None)
    } else {
        let token = ctx.gh.installation_token(installation_id).await?;
        (
            format!("https://github.com/{owner}/{repo}.git"),
            Some(token),
        )
    };
    gitutil::clone_bare(&url, &dest, bearer.as_deref())
        .await
        .map_err(|e| e.to_string())?;
    let _ = gitutil::fetch_shas(&dest, &[&pr.head.sha, &pr.base.sha], bearer.as_deref()).await;

    let (tree, paths, code) = gitutil::merge_tree(&dest, &pr.base.sha, &pr.head.sha)
        .await
        .map_err(|e| e.to_string())?;
    if code == 0 {
        return Ok("no conflicts to fight".into());
    }

    let hunks = match gitutil::collect_hunks(&dest, &tree, &pr.base.sha, &paths).await {
        Ok(h) => h,
        Err(gitutil::GitError::TooMany(n)) => {
            return Ok(format!(
                "too many conflicts for one fight ({n}; max {MAX_HUNKS})"
            ));
        }
        Err(gitutil::GitError::NothingToFight) => {
            return Ok("the conflicts are not the kind git fight can play".into());
        }
        Err(e) => return Err(e.to_string()),
    };
    if hunks.len() > MAX_HUNKS {
        return Ok(format!(
            "too many conflicts for one fight ({}; max {MAX_HUNKS})",
            hunks.len()
        ));
    }

    let ours_login = pr.user.login.clone();
    let mut login_cache: HashMap<String, Option<String>> = HashMap::new();
    let mut sides: Vec<(String, String, Option<String>)> = Vec::new();
    for h in &hunks {
        let login = blame_login(
            &ctx.gh,
            installation_id,
            owner,
            repo,
            &h.blame_sha,
            &h.blame_email,
            &mut login_cache,
        )
        .await;
        sides.push(side_from_blame(&ours_login, login, &h.blame_name));
    }
    let (theirs_kind, theirs_name, theirs_login) = sides
        .first()
        .cloned()
        .unwrap_or_else(|| ("cpu".into(), "theirs".into(), None));

    let id = uuid::Uuid::new_v4().simple().to_string();
    let seed = uuid::Uuid::new_v4().as_u128() as u64;
    let ours_token = uuid::Uuid::new_v4().simple().to_string();
    let theirs_token = uuid::Uuid::new_v4().simple().to_string();
    db::insert_full_match(
        &ctx.pool,
        &NewMatch {
            id: id.clone(),
            seed,
            delay: INPUT_DELAY,
            ours_name: ours_login.clone(),
            theirs_name: theirs_name.clone(),
            ours_kind: if theirs_kind == "mirror" {
                "mirror"
            } else {
                "github"
            }
            .into(),
            theirs_kind: theirs_kind.clone(),
            ours_login: Some(ours_login.clone()),
            theirs_login,
            ours_token,
            theirs_token,
            expire_secs: ctx.expire_secs,
            installation_id: Some(installation_id as i64),
            owner: owner.into(),
            repo: repo.into(),
            pr_number: number as i64,
            pr_head_sha: pr.head.sha.clone(),
            pr_base_sha: pr.base.sha.clone(),
        },
    )
    .await
    .map_err(|e| e.to_string())?;

    for (round, h) in hunks.iter().enumerate() {
        let ours_author = gitutil::latest_author(&dest, &pr.head.sha, &h.path)
            .await
            .unwrap_or_else(|| ours_login.clone());
        let ours_stats = gitutil::fighter_stats(&dest, &pr.head.sha, &h.path, &ours_author).await;
        let theirs_stats =
            gitutil::fighter_stats(&dest, &pr.base.sha, &h.path, &h.blame_name).await;
        db::insert_hunk(
            &ctx.pool,
            &db::NewHunk {
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
            },
        )
        .await
        .map_err(|e| e.to_string())?;
    }

    let rounds = hunks.len();
    let vs = if theirs_kind == "cpu" {
        format!("{ours_login} vs {theirs_name} (CPU)")
    } else if theirs_kind == "mirror" {
        format!("{ours_login} vs {ours_login} (mirror)")
    } else {
        format!("{ours_login} vs {theirs_name}")
    };
    let link = format!("{}/match/{id}", ctx.public_url.trim_end_matches('/'));
    Ok(format!(
        "git fight: {vs}. {rounds} round{}. {link}",
        if rounds == 1 { "" } else { "s" }
    ))
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
) -> Option<String> {
    let key = if !sha.is_empty() {
        format!("s:{sha}")
    } else {
        format!("e:{email}")
    };
    if let Some(hit) = cache.get(&key) {
        return hit.clone();
    }
    let mut login = gh.login_for_commit(installation_id, owner, repo, sha).await;
    if login.is_none() {
        login = gh
            .login_for_email(installation_id, owner, repo, email)
            .await;
    }
    cache.insert(key, login.clone());
    if !email.is_empty() {
        cache.entry(format!("e:{email}")).or_insert(login.clone());
    }
    login
}

fn side_from_blame(
    ours_login: &str,
    login: Option<String>,
    blame_name: &str,
) -> (String, String, Option<String>) {
    match login {
        Some(l) if l == ours_login => ("mirror".into(), ours_login.to_string(), Some(l)),
        Some(l) => ("github".into(), l.clone(), Some(l)),
        None => ("cpu".into(), blame_name.to_string(), None),
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
}
