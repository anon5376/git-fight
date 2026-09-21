//! Git plumbing with hooks disabled. Never a shell, never user-repo code.

use crate::limits::{CLONE_TIMEOUT, MAX_BLOB_BYTES, MAX_HUNKS};
use git_fight_core::{ConflictFile, FighterStats};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Clone, Debug)]
pub struct FightHunk {
    pub path: String,
    pub hunk_index: usize,
    pub ours: Vec<u8>,
    pub theirs: Vec<u8>,
    pub base: Vec<u8>,
    pub blame_name: String,
    pub blame_email: String,
    pub blame_sha: String,
}

#[derive(Debug)]
pub enum GitError {
    Timeout,
    Command(String),
    TooMany(usize),
    NothingToFight,
    Io(std::io::Error),
}

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GitError::Timeout => write!(f, "git timed out"),
            GitError::Command(s) => write!(f, "{s}"),
            GitError::TooMany(n) => write!(f, "too many conflicts ({n})"),
            GitError::NothingToFight => write!(f, "no fightable hunks"),
            GitError::Io(e) => write!(f, "{e}"),
        }
    }
}

pub fn is_safe_path(path: &str) -> bool {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains('\0') {
        return false;
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return false;
    }
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                if s == OsStr::new(".git") || s.is_empty() {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn git_base() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_CONFIG_GLOBAL", "/dev/null");
    c.env("GIT_CONFIG_NOSYSTEM", "1");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env_remove("GIT_DIR");
    c.env_remove("GIT_WORK_TREE");
    c.arg("-c").arg("core.hooksPath=/dev/null");
    c.kill_on_drop(true);
    c.stdin(Stdio::null());
    c
}

async fn run(mut cmd: Command, limit: Duration) -> Result<(i32, Vec<u8>, Vec<u8>), GitError> {
    let out = timeout(limit, cmd.output())
        .await
        .map_err(|_| GitError::Timeout)?
        .map_err(GitError::Io)?;
    Ok((out.status.code().unwrap_or(-1), out.stdout, out.stderr))
}

async fn run_stdin(
    mut cmd: Command,
    input: &[u8],
    limit: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>), GitError> {
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(GitError::Io)?;
    if let Some(mut stdin) = child.stdin.take() {
        timeout(limit, stdin.write_all(input))
            .await
            .map_err(|_| GitError::Timeout)?
            .map_err(GitError::Io)?;
    }
    let out = timeout(limit, child.wait_with_output())
        .await
        .map_err(|_| GitError::Timeout)?
        .map_err(GitError::Io)?;
    Ok((out.status.code().unwrap_or(-1), out.stdout, out.stderr))
}

fn apply_auth(cmd: &mut Command, url: &str, bearer: Option<&str>) {
    if url.starts_with("file://") || Path::new(url).is_absolute() {
        cmd.arg("-c").arg("protocol.file.allow=always");
    }
    if let Some(token) = bearer {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: bearer {token}"));
    }
}

fn redact_git_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.lines() {
        let lower = line.to_ascii_lowercase();
        if lower.contains("authorization:") || lower.contains("x-access-token:") {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

pub async fn clone_bare(url: &str, dest: &Path, bearer: Option<&str>) -> Result<(), GitError> {
    let mut cmd = git_base();
    apply_auth(&mut cmd, url, bearer);
    cmd.args([
        "clone",
        "--bare",
        "--filter=blob:none",
        url,
        dest.to_str()
            .ok_or_else(|| GitError::Command("dest".into()))?,
    ]);
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(())
}

fn git_dir(dir: &Path) -> Command {
    let mut c = git_base();
    c.arg("--git-dir").arg(dir);
    c
}

pub async fn fetch_shas(dir: &Path, shas: &[&str], bearer: Option<&str>) -> Result<(), GitError> {
    let mut cmd = git_dir(dir);
    if let Some(token) = bearer {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: bearer {token}"));
    }
    cmd.arg("fetch").arg("origin").args(shas.iter().copied());
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        let msg = redact_git_text(&String::from_utf8_lossy(&err));
        if msg.contains("couldn't find remote ref") || msg.contains("unable to find") {
            return Ok(());
        }
        return Err(GitError::Command(msg));
    }
    Ok(())
}

fn split_tree_oid(stdout: &[u8]) -> (&[u8], &[u8], bool) {
    for (i, b) in stdout.iter().copied().enumerate() {
        if b == 0 || b == b'\n' {
            return (&stdout[..i], &stdout[i + 1..], b == 0);
        }
    }
    (stdout, &[], false)
}

fn path_from_info_record(rec: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(rec).ok()?.trim();
    if line.is_empty() || line.starts_with("Auto-merging") || line.starts_with("CONFLICT") {
        return None;
    }
    let (_, rest) = line.split_once('\t')?;
    Some(unquote_git_path(rest.trim()))
}

pub fn parse_merge_tree_output(stdout: &[u8]) -> Result<(String, BTreeSet<String>), GitError> {
    let (tree_bytes, rest, nul) = split_tree_oid(stdout);
    let tree = String::from_utf8_lossy(tree_bytes).trim().to_string();
    if tree.len() != 40 && tree.len() != 64 {
        return Err(GitError::Command("merge-tree: missing tree oid".into()));
    }
    let mut paths = BTreeSet::new();
    if nul {
        for rec in rest.split(|b| *b == 0) {
            if rec.is_empty() {
                break;
            }
            let Some(path) = path_from_info_record(rec) else {
                break;
            };
            if is_safe_path(&path) {
                paths.insert(path);
            }
        }
    } else {
        for line in String::from_utf8_lossy(rest).split('\n') {
            if line.is_empty() {
                break;
            }
            let Some(path) = path_from_info_record(line.as_bytes()) else {
                break;
            };
            if is_safe_path(&path) {
                paths.insert(path);
            }
        }
    }
    Ok((tree, paths))
}

fn unquote_git_path(s: &str) -> String {
    if let Some(inner) = s.strip_prefix('"').and_then(|t| t.strip_suffix('"')) {
        inner
            .replace("\\t", "\t")
            .replace("\\n", "\n")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        s.to_string()
    }
}

/// `git merge-tree --write-tree <base> <head>` so git-ours is the base branch
/// and git-theirs is the PR. Game Ours is the PR; callers swap when collecting hunks.
pub async fn merge_tree(
    dir: &Path,
    base: &str,
    head: &str,
) -> Result<(String, BTreeSet<String>, i32), GitError> {
    let mut cmd = git_dir(dir);
    cmd.args(["merge-tree", "--write-tree", "-z", base, head]);
    let (code, out, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 && code != 1 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    let (tree, paths) = parse_merge_tree_output(&out)?;
    Ok((tree, paths, code))
}

async fn ls_tree_mode(dir: &Path, tree: &str, path: &str) -> Result<Option<String>, GitError> {
    let mut cmd = git_dir(dir);
    cmd.args(["ls-tree", tree, "--", path]);
    let (code, out, _) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Ok(None);
    }
    let line = String::from_utf8_lossy(&out);
    let mode = line.split_whitespace().next().unwrap_or("");
    if mode.is_empty() {
        return Ok(None);
    }
    Ok(Some(mode.to_string()))
}

async fn cat_file(dir: &Path, spec: &str) -> Result<Vec<u8>, GitError> {
    let mut cmd = git_dir(dir);
    cmd.args(["cat-file", "blob", spec]);
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(
            String::from_utf8_lossy(&err).into_owned(),
        ));
    }
    Ok(out)
}

pub async fn collect_hunks(
    dir: &Path,
    tree: &str,
    base_sha: &str,
    paths: &BTreeSet<String>,
) -> Result<Vec<FightHunk>, GitError> {
    let mut out = Vec::new();
    for path in paths {
        if !is_safe_path(path) {
            continue;
        }
        let Some(mode) = ls_tree_mode(dir, tree, path).await? else {
            continue;
        };
        if mode != "100644" && mode != "100755" {
            continue;
        }
        let blob = cat_file(dir, &format!("{tree}:{path}")).await?;
        if blob.len() > MAX_BLOB_BYTES {
            continue;
        }
        let Ok(parsed) = ConflictFile::parse(&blob) else {
            continue;
        };
        let base_file = cat_file(dir, &format!("{base_sha}:{path}"))
            .await
            .unwrap_or_default();
        for i in 0..parsed.hunk_count() {
            // merge-tree <base> <head>: git-ours is base, git-theirs is the PR.
            let game_ours = parsed.theirs(i).to_vec();
            let game_theirs = parsed.ours(i).to_vec();
            let (name, email, sha) =
                blame_theirs(dir, base_sha, path, &base_file, &game_theirs).await;
            out.push(FightHunk {
                path: path.clone(),
                hunk_index: i,
                ours: game_ours,
                theirs: game_theirs,
                base: parsed.base(i).unwrap_or(&[]).to_vec(),
                blame_name: name,
                blame_email: email,
                blame_sha: sha,
            });
            if out.len() > MAX_HUNKS {
                return Err(GitError::TooMany(out.len()));
            }
        }
    }
    if out.is_empty() {
        return Err(GitError::NothingToFight);
    }
    if out.len() > MAX_HUNKS {
        return Err(GitError::TooMany(out.len()));
    }
    Ok(out)
}

async fn blame_theirs(
    dir: &Path,
    base_sha: &str,
    path: &str,
    base_file: &[u8],
    needle: &[u8],
) -> (String, String, String) {
    let range = line_range(base_file, needle);
    let mut cmd = git_dir(dir);
    cmd.arg("blame").arg("--line-porcelain");
    if let Some((a, b)) = range {
        cmd.arg("-L").arg(format!("{a},{b}"));
    }
    cmd.args([base_sha, "--", path]);
    if let Ok((0, out, _)) = run(cmd, Duration::from_secs(20)).await {
        return parse_blame_author(&out);
    }
    fallback_author(dir, base_sha, path).await
}

fn line_range(haystack: &[u8], needle: &[u8]) -> Option<(usize, usize)> {
    if needle.is_empty() || haystack.is_empty() {
        return None;
    }
    let pos = haystack.windows(needle.len()).position(|w| w == needle)?;
    let start = haystack[..pos].iter().filter(|b| **b == b'\n').count() + 1;
    let nlines = needle.iter().filter(|b| **b == b'\n').count().max(1);
    Some((start, start + nlines - 1))
}

fn parse_blame_author(porcelain: &[u8]) -> (String, String, String) {
    let text = String::from_utf8_lossy(porcelain);
    let mut sha = String::new();
    let mut name = String::new();
    let mut email = String::new();
    for line in text.lines() {
        if sha.is_empty()
            && line.len() >= 40
            && line.as_bytes().iter().take(40).all(u8::is_ascii_hexdigit)
        {
            sha = line.split_whitespace().next().unwrap_or("").to_string();
        }
        if let Some(rest) = line.strip_prefix("author ") {
            if name.is_empty() {
                name = rest.to_string();
            }
        }
        if let Some(rest) = line.strip_prefix("author-mail ") {
            if email.is_empty() {
                email = rest
                    .trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string();
            }
        }
        if !name.is_empty() && !email.is_empty() && !sha.is_empty() {
            break;
        }
    }
    if name.is_empty() {
        name = "theirs".into();
    }
    (name, email, sha)
}

async fn fallback_author(dir: &Path, base_sha: &str, path: &str) -> (String, String, String) {
    let mut cmd = git_dir(dir);
    cmd.args(["log", "-1", "--format=%an%n%ae%n%H", base_sha, "--", path]);
    if let Ok((0, out, _)) = run(cmd, Duration::from_secs(10)).await {
        let text = String::from_utf8_lossy(&out);
        let mut lines = text.lines();
        let name = lines.next().unwrap_or("theirs").to_string();
        let email = lines.next().unwrap_or("").to_string();
        let sha = lines.next().unwrap_or(base_sha).to_string();
        return (name, email, sha);
    }
    ("theirs".into(), String::new(), base_sha.into())
}

fn is_safe_rev(rev: &str) -> bool {
    let n = rev.len();
    (8..=64).contains(&n) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

fn looks_like_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec")
}

/// Latest commit author on `rev` that touched `path` (`git log -1 --format=%an`).
pub async fn latest_author(dir: &Path, rev: &str, path: &str) -> Option<String> {
    if !is_safe_rev(rev) || !is_safe_path(path) {
        return None;
    }
    let mut cmd = git_dir(dir);
    cmd.args(["log", "-1", "--format=%an", rev, "--", path]);
    let (code, out, _) = run(cmd, Duration::from_secs(10)).await.ok()?;
    if code != 0 {
        return None;
    }
    let name = String::from_utf8_lossy(&out).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// CLI-equivalent HP / armor / special from git history. Falls back to defaults.
pub async fn fighter_stats(dir: &Path, rev: &str, path: &str, author: &str) -> FighterStats {
    if !is_safe_rev(rev) || !is_safe_path(path) {
        return FighterStats::default();
    }
    let hp = hp_from_blame(dir, rev, path, author).await;
    let armor = armor_from_commit(dir, rev, path).await;
    let special = special_from_log(dir, rev, author).await;
    FighterStats::clamped(hp, armor, special)
}

async fn hp_from_blame(dir: &Path, rev: &str, path: &str, name: &str) -> i32 {
    let _ = cat_file(dir, &format!("{rev}:{path}")).await;
    let mut cmd = git_dir(dir);
    cmd.args(["blame", "--line-porcelain", rev, "--", path]);
    let Ok((0, out, _)) = run(cmd, Duration::from_secs(20)).await else {
        return 100;
    };
    let text = String::from_utf8_lossy(&out);
    let mut mine = 0i32;
    let mut total = 0i32;
    for line in text.lines() {
        if let Some(author) = line.strip_prefix("author ") {
            total += 1;
            if author == name {
                mine += 1;
            }
        }
    }
    if total <= 0 {
        return 100;
    }
    80 + (mine * 40) / total
}

async fn armor_from_commit(dir: &Path, rev: &str, path: &str) -> bool {
    let mut cmd = git_dir(dir);
    cmd.args(["log", "-1", "--format=%H", rev, "--", path]);
    let Ok((0, out, _)) = run(cmd, Duration::from_secs(10)).await else {
        return false;
    };
    let commit = String::from_utf8_lossy(&out).trim().to_string();
    if !is_safe_rev(&commit) {
        return false;
    }
    let mut cmd = git_dir(dir);
    cmd.args([
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "-r",
        "--root",
        &commit,
    ]);
    let Ok((0, out, _)) = run(cmd, Duration::from_secs(10)).await else {
        return false;
    };
    String::from_utf8_lossy(&out).lines().any(looks_like_test)
}

async fn special_from_log(dir: &Path, rev: &str, name: &str) -> bool {
    let name = name.trim();
    if name.is_empty() || name.contains('\0') || name.contains('\n') || name.starts_with('-') {
        return false;
    }
    if !is_safe_rev(rev) {
        return false;
    }
    let mut cmd = git_dir(dir);
    cmd.args(["log", "--since=7 days ago", "--format=%ad", "--date=short"]);
    cmd.arg(format!("--author={name}"));
    cmd.arg(rev);
    let Ok((0, out, _)) = run(cmd, Duration::from_secs(15)).await else {
        return false;
    };
    let mut days = BTreeSet::new();
    for line in String::from_utf8_lossy(&out).lines() {
        if !line.is_empty() {
            days.insert(line.to_string());
        }
    }
    days.len() >= 3
}

/// `git-fight/pr-<number>-<match-id>` only. Never main, never an existing user branch.
pub fn result_ref(pr_number: i64, match_id: &str) -> Result<String, GitError> {
    if pr_number <= 0 {
        return Err(GitError::Command("missing pull request".into()));
    }
    if match_id.is_empty()
        || match_id.len() > 64
        || !match_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(GitError::Command("unsafe match id".into()));
    }
    Ok(format!("git-fight/pr-{pr_number}-{match_id}"))
}

pub async fn hash_object_w(dir: &Path, bytes: &[u8]) -> Result<String, GitError> {
    let mut cmd = git_dir(dir);
    cmd.args(["hash-object", "-w", "--stdin"]);
    let (code, out, err) = run_stdin(cmd, bytes, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(
            String::from_utf8_lossy(&err).into_owned(),
        ));
    }
    let oid = String::from_utf8_lossy(&out).trim().to_string();
    if oid.len() != 40 && oid.len() != 64 {
        return Err(GitError::Command("hash-object: bad oid".into()));
    }
    Ok(oid)
}

fn with_index(cmd: &mut Command, index: &Path) {
    cmd.env("GIT_INDEX_FILE", index);
}

pub async fn read_tree_index(dir: &Path, index: &Path, tree: &str) -> Result<(), GitError> {
    let mut cmd = git_dir(dir);
    with_index(&mut cmd, index);
    cmd.args(["read-tree", tree]);
    let (code, _, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(())
}

pub async fn update_index_cacheinfo(
    dir: &Path,
    index: &Path,
    mode: &str,
    blob: &str,
    path: &str,
) -> Result<(), GitError> {
    if !is_safe_path(path) {
        return Err(GitError::Command("unsafe path".into()));
    }
    if mode != "100644" && mode != "100755" {
        return Err(GitError::Command("refusing non-regular mode".into()));
    }
    let mut cmd = git_dir(dir);
    with_index(&mut cmd, index);
    cmd.args(["update-index", "--add", "--cacheinfo", mode, blob, path]);
    let (code, _, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(())
}

pub async fn write_tree_index(dir: &Path, index: &Path) -> Result<String, GitError> {
    let mut cmd = git_dir(dir);
    with_index(&mut cmd, index);
    cmd.arg("write-tree");
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

pub async fn commit_tree(
    dir: &Path,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Result<String, GitError> {
    let mut cmd = git_dir(dir);
    cmd.env("GIT_AUTHOR_NAME", "git-fight");
    cmd.env("GIT_AUTHOR_EMAIL", "git-fight@users.noreply.github.com");
    cmd.env("GIT_COMMITTER_NAME", "git-fight");
    cmd.env("GIT_COMMITTER_EMAIL", "git-fight@users.noreply.github.com");
    cmd.args(["commit-tree", tree]);
    for p in parents {
        cmd.arg("-p").arg(p);
    }
    let (code, out, err) = run_stdin(cmd, message.as_bytes(), Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

pub async fn ref_exists(url: &str, refname: &str, bearer: Option<&str>) -> Result<bool, GitError> {
    if !refname.starts_with("git-fight/") {
        return Err(GitError::Command(
            "refusing to inspect non git-fight ref".into(),
        ));
    }
    let mut cmd = git_base();
    apply_auth(&mut cmd, url, bearer);
    cmd.args([
        "ls-remote",
        "--heads",
        url,
        &format!("refs/heads/{refname}"),
    ]);
    let (code, out, err) = run(cmd, Duration::from_secs(20)).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(!String::from_utf8_lossy(&out).trim().is_empty())
}

/// Create-only push of `commit` to `refs/heads/<refname>`. Never force-pushes.
pub async fn push_create_only(
    dir: &Path,
    url: &str,
    commit: &str,
    refname: &str,
    bearer: Option<&str>,
) -> Result<(), GitError> {
    if !refname.starts_with("git-fight/") || refname.contains("..") || refname.contains('\\') {
        return Err(GitError::Command(
            "refusing to push outside git-fight/*".into(),
        ));
    }
    if ref_exists(url, refname, bearer).await? {
        return Err(GitError::Command(format!(
            "ref refs/heads/{refname} already exists"
        )));
    }
    let dest = format!("{commit}:refs/heads/{refname}");
    let mut cmd = git_dir(dir);
    apply_auth(&mut cmd, url, bearer);
    cmd.args(["push", "--", url, &dest]);
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        return Err(GitError::Command(redact_git_text(
            &String::from_utf8_lossy(&err),
        )));
    }
    Ok(())
}

pub fn temp_index_path(dir: &Path) -> PathBuf {
    dir.join(".git-fight-index")
}

/// Replace resolved regular files in `merge_tree` and write a new tree. Plumbing only.
pub async fn build_resolved_tree(
    dir: &Path,
    merge_tree: &str,
    files: &[(String, Vec<u8>)],
) -> Result<String, GitError> {
    let index = temp_index_path(dir);
    let _ = tokio::fs::remove_file(&index).await;
    read_tree_index(dir, &index, merge_tree).await?;
    for (path, bytes) in files {
        if !is_safe_path(path) {
            continue;
        }
        let mode = ls_tree_mode(dir, merge_tree, path)
            .await?
            .unwrap_or_else(|| "100644".into());
        if mode != "100644" && mode != "100755" {
            continue;
        }
        let blob = hash_object_w(dir, bytes).await?;
        update_index_cacheinfo(dir, &index, &mode, &blob, path).await?;
    }
    let tree = write_tree_index(dir, &index).await?;
    let _ = tokio::fs::remove_file(&index).await;
    Ok(tree)
}

pub async fn cat_blob(dir: &Path, spec: &str) -> Result<Vec<u8>, GitError> {
    cat_file(dir, spec).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_dotgit_and_parent() {
        assert!(!is_safe_path("../x"));
        assert!(!is_safe_path(".git/config"));
        assert!(!is_safe_path("/etc/passwd"));
        assert!(!is_safe_path("foo/.git/bar"));
        assert!(is_safe_path("src/lib.rs"));
        assert!(is_safe_path("a/b.c"));
    }

    #[test]
    fn test_paths_match_cli() {
        assert!(looks_like_test("src/foo_test.rs"));
        assert!(looks_like_test("web/spec/a.ts"));
        assert!(!looks_like_test("src/lib.rs"));
        assert!(is_safe_rev("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_safe_rev("HEAD"));
        assert!(!is_safe_rev("../main"));
    }

    #[test]
    fn result_ref_only_git_fight() {
        assert_eq!(
            result_ref(7, "deadbeef").unwrap(),
            "git-fight/pr-7-deadbeef"
        );
        assert!(result_ref(0, "deadbeef").is_err());
        assert!(result_ref(1, "../main").is_err());
        assert!(result_ref(1, "MAIN").is_err());
        assert!(result_ref(1, "dead/beef").is_err());
    }

    #[test]
    fn parse_conflict_paths() {
        let sample = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
100644 1111111111111111111111111111111111111111 1\tlib.rs\n\
100644 2222222222222222222222222222222222222222 2\tlib.rs\n\
100644 3333333333333333333333333333333333333333 3\tlib.rs\n\
\n\
Auto-merging lib.rs\n";
        let (tree, paths) = parse_merge_tree_output(sample).unwrap();
        assert_eq!(tree.len(), 40);
        assert!(paths.contains("lib.rs"));
    }

    #[test]
    fn parse_conflict_paths_nul() {
        let mut sample = Vec::new();
        sample.extend_from_slice(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        sample.push(0);
        sample.extend_from_slice(b"100644 1111111111111111111111111111111111111111 1\tlib.rs");
        sample.push(0);
        sample.extend_from_slice(b"100644 2222222222222222222222222222222222222222 2\tlib.rs");
        sample.push(0);
        sample.extend_from_slice(b"100644 3333333333333333333333333333333333333333 3\tlib.rs");
        sample.push(0);
        sample.push(0);
        let (tree, paths) = parse_merge_tree_output(&sample).unwrap();
        assert_eq!(tree.len(), 40);
        assert!(paths.contains("lib.rs"));
    }
}
