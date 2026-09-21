//! Git plumbing with hooks disabled. Never a shell, never user-repo code.

use crate::limits::{CLONE_TIMEOUT, MAX_BLOB_BYTES, MAX_CONFLICT_PATHS, MAX_HUNKS};
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

fn ssl_ca_bundle() -> Option<&'static str> {
    const CA: &str = "/etc/ssl/certs/ca-certificates.crt";
    Path::new(CA).is_file().then_some(CA)
}

fn git_base() -> Command {
    let mut c = Command::new("git");
    c.env("GIT_CONFIG_GLOBAL", "/dev/null");
    c.env("GIT_CONFIG_NOSYSTEM", "1");
    c.env("GIT_TERMINAL_PROMPT", "0");
    c.env_remove("GIT_DIR");
    c.env_remove("GIT_WORK_TREE");
    // Inherited env can disable TLS or dump Authorization: on stderr.
    c.env_remove("GIT_SSL_NO_VERIFY");
    c.env_remove("GIT_CURL_VERBOSE");
    c.env_remove("GIT_TRACE");
    c.env_remove("GIT_TRACE_CURL");
    c.env_remove("GIT_TRACE_PACKET");
    c.env_remove("GIT_CONFIG_PARAMETERS");
    c.env_remove("GIT_CONFIG_COUNT");
    c.env_remove("GIT_PROXY_COMMAND");
    c.env_remove("GIT_SSH_COMMAND");
    c.env_remove("GIT_ASKPASS");
    // Clone URLs are https://github.com/… or file:// tests. No ssh/ext/git.
    c.env("GIT_ALLOW_PROTOCOL", "https:file");
    if let Some(ca) = ssl_ca_bundle() {
        c.env("GIT_SSL_CAINFO", ca);
        c.arg("-c").arg(format!("http.sslCAInfo={ca}"));
    }
    c.arg("-c").arg("core.hooksPath=/dev/null");
    c.arg("-c").arg("http.sslVerify=true");
    c.kill_on_drop(true);
    c.stdin(Stdio::null());
    // Never inherit a user worktree as cwd (hooks, local config, relative dest).
    c.current_dir("/");
    c
}

/// git clone/fetch spawn index-pack. Put the child in its own group so a
/// 60s timeout can SIGKILL helpers, not only the git parent.
fn prepare_child(cmd: &mut Command) {
    #[cfg(unix)]
    cmd.process_group(0);
}

fn kill_process_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        if let Ok(pgid) = i32::try_from(pid) {
            if pgid > 1 {
                // SAFETY: process_group(0) made this pid the group leader.
                // Negative pgid signals that group (git + index-pack), never -1.
                unsafe {
                    libc::kill(-pgid, libc::SIGKILL);
                }
            }
        }
    }
}

async fn wait_child(
    child: tokio::process::Child,
    limit: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>), GitError> {
    let pid = child.id();
    match timeout(limit, child.wait_with_output()).await {
        Ok(out) => {
            let out = out.map_err(GitError::Io)?;
            Ok((out.status.code().unwrap_or(-1), out.stdout, out.stderr))
        }
        Err(_) => {
            kill_process_group(pid);
            Err(GitError::Timeout)
        }
    }
}

async fn run(mut cmd: Command, limit: Duration) -> Result<(i32, Vec<u8>, Vec<u8>), GitError> {
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    prepare_child(&mut cmd);
    let child = cmd.spawn().map_err(GitError::Io)?;
    wait_child(child, limit).await
}

async fn run_stdin(
    mut cmd: Command,
    input: &[u8],
    limit: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>), GitError> {
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    prepare_child(&mut cmd);
    let mut child = cmd.spawn().map_err(GitError::Io)?;
    let pid = child.id();
    if let Some(mut stdin) = child.stdin.take() {
        match timeout(limit, stdin.write_all(input)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(GitError::Io(e)),
            Err(_) => {
                kill_process_group(pid);
                let _ = child.start_kill();
                return Err(GitError::Timeout);
            }
        }
    }
    wait_child(child, limit).await
}

fn apply_auth(cmd: &mut Command, url: &str, bearer: Option<&str>) {
    if url.starts_with("file://") || Path::new(url).is_absolute() {
        cmd.arg("-c").arg("protocol.file.allow=always");
    }
    apply_git_bearer(cmd, bearer);
}

/// GitHub App git HTTPS uses Basic `x-access-token:<installation token>`, not REST Bearer.
/// Scoped to github.com so a redirect cannot collect the installation token.
fn github_git_auth_header(token: &str) -> String {
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("x-access-token:{token}"),
    );
    format!("http.https://github.com/.extraHeader=Authorization: Basic {basic}")
}

fn apply_git_bearer(cmd: &mut Command, bearer: Option<&str>) {
    if let Some(token) = bearer {
        // extraHeader + HTTP/2 can drop the GitHub App Basic header.
        cmd.arg("-c").arg("http.version=HTTP/1.1");
        cmd.arg("-c").arg(github_git_auth_header(token));
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

fn git_err(err: &[u8]) -> GitError {
    GitError::Command(redact_git_text(&String::from_utf8_lossy(err)))
}

fn is_safe_blob_spec(spec: &str) -> bool {
    match spec.split_once(':') {
        Some((rev, path)) => is_safe_rev(rev) && is_safe_path(path),
        None => is_safe_rev(spec),
    }
}

/// Clone/push remotes: test `file://` paths, or `https://github.com/<owner>/<repo>.git`.
fn is_safe_git_url(url: &str) -> bool {
    if !(8..=4096).contains(&url.len()) {
        return false;
    }
    if url
        .as_bytes()
        .iter()
        .any(|b| *b < 0x20 || *b > 0x7e || matches!(*b, b'?' | b'#' | b'\\' | b'@' | b' ' | b'\t'))
    {
        return false;
    }
    if let Some(rest) = url.strip_prefix("file://") {
        return rest.starts_with('/') && rest.len() >= 2 && !rest.contains("//");
    }
    let Some(rest) = url.strip_prefix("https://github.com/") else {
        return false;
    };
    let Some(path) = rest.strip_suffix(".git") else {
        return false;
    };
    let mut parts = path.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) => {
            crate::gh::is_safe_github_name(owner) && crate::gh::is_safe_github_name(repo)
        }
        _ => false,
    }
}

pub async fn clone_bare(url: &str, dest: &Path, bearer: Option<&str>) -> Result<(), GitError> {
    if !is_safe_git_url(url) {
        return Err(GitError::Command("unsafe url".into()));
    }
    let dest_s = dest
        .to_str()
        .ok_or_else(|| GitError::Command("dest".into()))?;
    if dest_s.starts_with('-') {
        return Err(GitError::Command("unsafe dest".into()));
    }
    let mut cmd = git_base();
    apply_auth(&mut cmd, url, bearer);
    cmd.args([
        "clone",
        "--bare",
        "--filter=blob:none",
        "--no-tags",
        "--no-local",
        "--",
        url,
        dest_s,
    ]);
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(())
}

fn git_dir(dir: &Path, bearer: Option<&str>) -> Command {
    let mut c = git_base();
    c.arg("--git-dir").arg(dir);
    // Partial clones lazy-fetch blobs over the origin URL (file:// tests, GitHub HTTPS).
    c.arg("-c").arg("protocol.file.allow=always");
    apply_git_bearer(&mut c, bearer);
    c
}

pub async fn fetch_shas(dir: &Path, shas: &[&str], bearer: Option<&str>) -> Result<(), GitError> {
    if shas.iter().any(|s| !is_safe_rev(s)) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["fetch", "--no-tags", "origin"]);
    cmd.args(shas.iter().copied());
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

fn is_safe_refname(s: &str) -> bool {
    let n = s.len();
    (1..=255).contains(&n)
        && !s.starts_with('-')
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.ends_with('.')
        && !s.contains("..")
        && !s.contains("//")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/'))
}

async fn fetch_refspec(dir: &Path, refspec: &str, bearer: Option<&str>) -> Result<(), GitError> {
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["fetch", "--no-tags", "origin", refspec]);
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(())
}

async fn ensure_commit(dir: &Path, sha: &str, bearer: Option<&str>) -> Result<(), GitError> {
    if !is_safe_rev(sha) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["cat-file", "-t", sha]);
    let (code, out, err) = run(cmd, Duration::from_secs(20)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    if String::from_utf8_lossy(&out).trim() != "commit" {
        return Err(GitError::Command("not a commit".into()));
    }
    Ok(())
}

/// GitHub clone copies `refs/heads/*` and tags, not `refs/pull/<n>/head`.
/// Fork PR heads (and same-repo heads that are not a branch name we cloned)
/// only exist on that pull ref. Fetch it, then require both SHAs to exist.
pub async fn fetch_pr_objects(
    dir: &Path,
    pr_number: u64,
    head_sha: &str,
    base_sha: &str,
    base_ref: Option<&str>,
    bearer: Option<&str>,
) -> Result<(), GitError> {
    if !is_safe_rev(head_sha) || !is_safe_rev(base_sha) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    if (1..=99_999_999).contains(&pr_number) {
        let spec = format!("+refs/pull/{pr_number}/head:refs/git-fight-fetch/head");
        let _ = fetch_refspec(dir, &spec, bearer).await;
    }
    if let Some(r) = base_ref {
        if is_safe_refname(r) {
            let spec = format!("+refs/heads/{r}:refs/git-fight-fetch/base");
            let _ = fetch_refspec(dir, &spec, bearer).await;
        }
    }
    let _ = fetch_shas(dir, &[head_sha, base_sha], bearer).await;
    ensure_commit(dir, head_sha, bearer).await?;
    ensure_commit(dir, base_sha, bearer).await?;
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
    bearer: Option<&str>,
) -> Result<(String, BTreeSet<String>, i32), GitError> {
    if !is_safe_rev(base) || !is_safe_rev(head) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["merge-tree", "--write-tree", "-z", base, head]);
    let (code, out, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 && code != 1 {
        return Err(git_err(&err));
    }
    let (tree, paths) = parse_merge_tree_output(&out)?;
    Ok((tree, paths, code))
}

async fn ls_tree_mode(
    dir: &Path,
    tree: &str,
    path: &str,
    bearer: Option<&str>,
) -> Result<Option<String>, GitError> {
    if !is_safe_rev(tree) || !is_safe_path(path) {
        return Ok(None);
    }
    let mut cmd = git_dir(dir, bearer);
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

async fn cat_file(dir: &Path, spec: &str, bearer: Option<&str>) -> Result<Vec<u8>, GitError> {
    if !is_safe_blob_spec(spec) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["cat-file", "blob", spec]);
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(out)
}

pub async fn collect_hunks(
    dir: &Path,
    tree: &str,
    base_sha: &str,
    paths: &BTreeSet<String>,
    bearer: Option<&str>,
) -> Result<Vec<FightHunk>, GitError> {
    if !is_safe_rev(tree) || !is_safe_rev(base_sha) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    if paths.len() > MAX_CONFLICT_PATHS {
        return Err(GitError::TooMany(paths.len()));
    }
    let mut out = Vec::new();
    for path in paths {
        if !is_safe_path(path) {
            continue;
        }
        let Some(mode) = ls_tree_mode(dir, tree, path, bearer).await? else {
            continue;
        };
        if mode != "100644" && mode != "100755" {
            continue;
        }
        let blob = cat_file(dir, &format!("{tree}:{path}"), bearer).await?;
        if blob.len() > MAX_BLOB_BYTES {
            continue;
        }
        let Ok(parsed) = ConflictFile::parse(&blob) else {
            continue;
        };
        let base_file = cat_file(dir, &format!("{base_sha}:{path}"), bearer)
            .await
            .unwrap_or_default();
        for i in 0..parsed.hunk_count() {
            // merge-tree <base> <head>: git-ours is base, git-theirs is the PR.
            let game_ours = parsed.theirs(i).to_vec();
            let game_theirs = parsed.ours(i).to_vec();
            let (name, email, sha) =
                blame_theirs(dir, base_sha, path, &base_file, &game_theirs, bearer).await;
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
    bearer: Option<&str>,
) -> (String, String, String) {
    if !is_safe_rev(base_sha) || !is_safe_path(path) {
        return ("theirs".into(), String::new(), String::new());
    }
    let range = line_range(base_file, needle);
    let mut cmd = git_dir(dir, bearer);
    cmd.arg("blame").arg("--line-porcelain");
    if let Some((a, b)) = range {
        cmd.arg("-L").arg(format!("{a},{b}"));
    }
    cmd.args([base_sha, "--", path]);
    if let Ok((0, out, _)) = run(cmd, Duration::from_secs(20)).await {
        return parse_blame_author(&out);
    }
    fallback_author(dir, base_sha, path, bearer).await
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

async fn fallback_author(
    dir: &Path,
    base_sha: &str,
    path: &str,
    bearer: Option<&str>,
) -> (String, String, String) {
    if !is_safe_rev(base_sha) || !is_safe_path(path) {
        return ("theirs".into(), String::new(), String::new());
    }
    let mut cmd = git_dir(dir, bearer);
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

pub fn is_safe_rev(rev: &str) -> bool {
    let n = rev.len();
    (8..=64).contains(&n) && rev.bytes().all(|b| b.is_ascii_hexdigit())
}

/// GitHub commit SHAs are 40 hex chars. Shorter or option-like values never reach git.
pub fn is_github_sha(rev: &str) -> bool {
    rev.len() == 40 && is_safe_rev(rev)
}

fn looks_like_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec")
}

/// Latest commit author on `rev` that touched `path` (`git log -1 --format=%an`).
pub async fn latest_author(
    dir: &Path,
    rev: &str,
    path: &str,
    bearer: Option<&str>,
) -> Option<String> {
    if !is_safe_rev(rev) || !is_safe_path(path) {
        return None;
    }
    let mut cmd = git_dir(dir, bearer);
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
pub async fn fighter_stats(
    dir: &Path,
    rev: &str,
    path: &str,
    author: &str,
    bearer: Option<&str>,
) -> FighterStats {
    if !is_safe_rev(rev) || !is_safe_path(path) {
        return FighterStats::default();
    }
    let hp = hp_from_blame(dir, rev, path, author, bearer).await;
    let armor = armor_from_commit(dir, rev, path, bearer).await;
    let special = special_from_log(dir, rev, author, bearer).await;
    FighterStats::clamped(hp, armor, special)
}

async fn hp_from_blame(dir: &Path, rev: &str, path: &str, name: &str, bearer: Option<&str>) -> i32 {
    let _ = cat_file(dir, &format!("{rev}:{path}"), bearer).await;
    let mut cmd = git_dir(dir, bearer);
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

async fn armor_from_commit(dir: &Path, rev: &str, path: &str, bearer: Option<&str>) -> bool {
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["log", "-1", "--format=%H", rev, "--", path]);
    let Ok((0, out, _)) = run(cmd, Duration::from_secs(10)).await else {
        return false;
    };
    let commit = String::from_utf8_lossy(&out).trim().to_string();
    if !is_safe_rev(&commit) {
        return false;
    }
    let mut cmd = git_dir(dir, bearer);
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

async fn special_from_log(dir: &Path, rev: &str, name: &str, bearer: Option<&str>) -> bool {
    let name = name.trim();
    if name.is_empty() || name.contains('\0') || name.contains('\n') || name.starts_with('-') {
        return false;
    }
    if !is_safe_rev(rev) {
        return false;
    }
    let mut cmd = git_dir(dir, bearer);
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
    if !crate::protocol::is_match_id(match_id) {
        return Err(GitError::Command("unsafe match id".into()));
    }
    Ok(format!("git-fight/pr-{pr_number}-{match_id}"))
}

pub async fn hash_object_w(
    dir: &Path,
    bytes: &[u8],
    bearer: Option<&str>,
) -> Result<String, GitError> {
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["hash-object", "-w", "--stdin"]);
    let (code, out, err) = run_stdin(cmd, bytes, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
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

pub async fn read_tree_index(
    dir: &Path,
    index: &Path,
    tree: &str,
    bearer: Option<&str>,
) -> Result<(), GitError> {
    if !is_safe_rev(tree) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    with_index(&mut cmd, index);
    cmd.args(["read-tree", tree]);
    let (code, _, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(())
}

pub async fn update_index_cacheinfo(
    dir: &Path,
    index: &Path,
    mode: &str,
    blob: &str,
    path: &str,
    bearer: Option<&str>,
) -> Result<(), GitError> {
    if !is_safe_path(path) {
        return Err(GitError::Command("unsafe path".into()));
    }
    if mode != "100644" && mode != "100755" {
        return Err(GitError::Command("refusing non-regular mode".into()));
    }
    if !is_safe_rev(blob) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    with_index(&mut cmd, index);
    cmd.args(["update-index", "--add", "--cacheinfo", mode, blob, path]);
    let (code, _, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(())
}

pub async fn write_tree_index(
    dir: &Path,
    index: &Path,
    bearer: Option<&str>,
) -> Result<String, GitError> {
    let mut cmd = git_dir(dir, bearer);
    with_index(&mut cmd, index);
    cmd.arg("write-tree");
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

pub async fn commit_tree(
    dir: &Path,
    tree: &str,
    parents: &[&str],
    message: &str,
    bearer: Option<&str>,
) -> Result<String, GitError> {
    if !is_safe_rev(tree) || parents.iter().any(|p| !is_safe_rev(p)) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
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
        return Err(git_err(&err));
    }
    Ok(String::from_utf8_lossy(&out).trim().to_string())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExistingResult {
    Missing,
    Ours,
    Foreign,
}

/// First line of a result commit plus parents `(pr_head, pr_base)` and author.
pub fn commit_is_match_result(raw: &str, match_id: &str, head: &str, base: &str) -> bool {
    if !crate::protocol::is_match_id(match_id) || !is_safe_rev(head) || !is_safe_rev(base) {
        return false;
    }
    let Some((headers, body)) = raw.split_once("\n\n") else {
        return false;
    };
    let mut parents = Vec::new();
    let mut author_ok = false;
    for line in headers.lines() {
        if let Some(p) = line.strip_prefix("parent ") {
            parents.push(p.trim());
        }
        if let Some(rest) = line.strip_prefix("author ") {
            author_ok = rest.starts_with("git-fight <git-fight@users.noreply.github.com>");
        }
    }
    if !author_ok || parents.len() != 2 {
        return false;
    }
    if !parents[0].eq_ignore_ascii_case(head) || !parents[1].eq_ignore_ascii_case(base) {
        return false;
    }
    let expect = format!("git fight match {match_id}");
    body.lines().next() == Some(expect.as_str())
}

async fn rev_parse_git_fight(
    dir: &Path,
    spec: &str,
    bearer: Option<&str>,
) -> Result<String, GitError> {
    let ok = spec == "refs/git-fight-fetch/result"
        || spec
            .strip_prefix("refs/heads/")
            .is_some_and(|r| r.starts_with("git-fight/") && is_safe_refname(r));
    if !ok {
        return Err(GitError::Command(
            "refusing to inspect non git-fight ref".into(),
        ));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["rev-parse", "--verify", spec]);
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    let sha = String::from_utf8_lossy(&out).trim().to_string();
    if !is_safe_rev(&sha) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    Ok(sha)
}

async fn cat_commit(dir: &Path, sha: &str, bearer: Option<&str>) -> Result<String, GitError> {
    if !is_safe_rev(sha) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["cat-file", "-p", sha]);
    let (code, out, err) = run(cmd, Duration::from_secs(15)).await?;
    if code != 0 {
        return Err(git_err(&err));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// After clone: the create-only ref is missing, already this match, or someone else's.
pub async fn inspect_result_ref(
    dir: &Path,
    url: &str,
    refname: &str,
    match_id: &str,
    head: &str,
    base: &str,
    bearer: Option<&str>,
) -> Result<ExistingResult, GitError> {
    if !refname.starts_with("git-fight/") || !is_safe_refname(refname) {
        return Err(GitError::Command(
            "refusing to inspect non git-fight ref".into(),
        ));
    }
    let local = format!("refs/heads/{refname}");
    let mut sha = rev_parse_git_fight(dir, &local, bearer).await.ok();
    let remote = ref_exists(url, refname, bearer).await?;
    if sha.is_none() && remote {
        let spec = format!("+refs/heads/{refname}:refs/git-fight-fetch/result");
        if fetch_refspec(dir, &spec, bearer).await.is_ok() {
            sha = rev_parse_git_fight(dir, "refs/git-fight-fetch/result", bearer)
                .await
                .ok();
        }
    }
    let Some(sha) = sha else {
        return Ok(if remote {
            ExistingResult::Foreign
        } else {
            ExistingResult::Missing
        });
    };
    match cat_commit(dir, &sha, bearer).await {
        Ok(raw) if commit_is_match_result(&raw, match_id, head, base) => Ok(ExistingResult::Ours),
        _ => Ok(ExistingResult::Foreign),
    }
}

pub async fn ref_exists(url: &str, refname: &str, bearer: Option<&str>) -> Result<bool, GitError> {
    if !is_safe_git_url(url) {
        return Err(GitError::Command("unsafe url".into()));
    }
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
        "--",
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
    if !is_safe_git_url(url) {
        return Err(GitError::Command("unsafe url".into()));
    }
    if !refname.starts_with("git-fight/") || refname.contains("..") || refname.contains('\\') {
        return Err(GitError::Command(
            "refusing to push outside git-fight/*".into(),
        ));
    }
    if !is_safe_rev(commit) {
        return Err(GitError::Command("unsafe revision".into()));
    }
    if ref_exists(url, refname, bearer).await? {
        return Err(GitError::Command(format!(
            "ref refs/heads/{refname} already exists"
        )));
    }
    let dest = format!("{commit}:refs/heads/{refname}");
    let mut cmd = git_dir(dir, bearer);
    cmd.args(["push", "--", url, &dest]);
    let (code, _, err) = run(cmd, CLONE_TIMEOUT).await?;
    if code != 0 {
        return Err(git_err(&err));
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
    bearer: Option<&str>,
) -> Result<String, GitError> {
    let index = temp_index_path(dir);
    let _ = tokio::fs::remove_file(&index).await;
    read_tree_index(dir, &index, merge_tree, bearer).await?;
    for (path, bytes) in files {
        if !is_safe_path(path) {
            continue;
        }
        let mode = ls_tree_mode(dir, merge_tree, path, bearer)
            .await?
            .unwrap_or_else(|| "100644".into());
        if mode != "100644" && mode != "100755" {
            continue;
        }
        let blob = hash_object_w(dir, bytes, bearer).await?;
        update_index_cacheinfo(dir, &index, &mode, &blob, path, bearer).await?;
    }
    let tree = write_tree_index(dir, &index, bearer).await?;
    let _ = tokio::fs::remove_file(&index).await;
    Ok(tree)
}

pub async fn cat_blob(dir: &Path, spec: &str, bearer: Option<&str>) -> Result<Vec<u8>, GitError> {
    cat_file(dir, spec, bearer).await
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
        assert!(is_github_sha("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_github_sha("aaaaaaaa"));
        assert!(!is_safe_rev("HEAD"));
        assert!(!is_github_sha("HEAD"));
        assert!(!is_safe_rev("../main"));
        assert!(!is_safe_rev("--upload-pack=true"));
        assert!(!is_safe_rev("-C"));
        assert!(is_safe_refname("main"));
        assert!(is_safe_refname("feat/foo-bar"));
        assert!(!is_safe_refname("--upload-pack=true"));
        assert!(!is_safe_refname("../main"));
        assert!(!is_safe_refname("heads//x"));
        assert!(is_safe_blob_spec(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:lib.rs"
        ));
        assert!(!is_safe_blob_spec("HEAD:lib.rs"));
        assert!(!is_safe_blob_spec("--upload-pack=true"));
        assert!(!is_safe_blob_spec(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:../x"
        ));
        assert!(is_safe_git_url("https://github.com/acme/box.git"));
        assert!(is_safe_git_url("file:///tmp/repo.git"));
        assert!(!is_safe_git_url("https://evil.example/acme/box.git"));
        assert!(!is_safe_git_url(
            "https://github.com.evil.example/acme/box.git"
        ));
        assert!(!is_safe_git_url("ssh://github.com/acme/box.git"));
        assert!(!is_safe_git_url("https://github.com/acme/box.git?u=1"));
        assert!(!is_safe_git_url("https://github.com/acme/../box.git"));
        assert!(!is_safe_git_url("--upload-pack=true"));
        assert!(!is_safe_git_url("git@github.com:acme/box.git"));
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
    fn result_commit_is_this_match_only() {
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let base = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let raw = format!(
            "tree {head}\nparent {head}\nparent {base}\nauthor git-fight <git-fight@users.noreply.github.com> 1 +0000\ncommitter git-fight <git-fight@users.noreply.github.com> 1 +0000\n\ngit fight match deadbeef\n\nround 1: lib.rs hunk 0 ours\n"
        );
        assert!(commit_is_match_result(&raw, "deadbeef", head, base));
        assert!(!commit_is_match_result(&raw, "otherid", head, base));
        assert!(!commit_is_match_result(&raw, "deadbeef", base, head));
        let human = raw.replace(
            "author git-fight <git-fight@users.noreply.github.com>",
            "author alice <alice@example.com>",
        );
        assert!(!commit_is_match_result(&human, "deadbeef", head, base));
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

    #[test]
    fn redacts_authorization_from_git_text() {
        let raw = "fatal: could not read\nAuthorization: bearer ghs_live_token\nAuthorization: Basic dGVzdA==\nx-access-token: abc\nerror: failed\n";
        let out = redact_git_text(raw);
        assert!(!out.to_ascii_lowercase().contains("authorization"));
        assert!(!out.contains("ghs_live_token"));
        assert!(!out.contains("dGVzdA=="));
        assert!(!out.contains("x-access-token"));
        assert!(out.contains("error: failed"));
    }

    #[tokio::test]
    async fn plumbing_keeps_tls_on() {
        let mut verify = git_base();
        verify.args(["config", "--get", "http.sslVerify"]);
        let (code, out, err) = run(verify, Duration::from_secs(5)).await.unwrap();
        assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
        assert_eq!(String::from_utf8_lossy(&out).trim(), "true");
        if let Some(ca) = ssl_ca_bundle() {
            let mut info = git_base();
            info.args(["config", "--get", "http.sslCAInfo"]);
            let (code, out, err) = run(info, Duration::from_secs(5)).await.unwrap();
            assert_eq!(code, 0, "{}", String::from_utf8_lossy(&err));
            assert_eq!(String::from_utf8_lossy(&out).trim(), ca);
        }
    }

    #[test]
    fn github_git_auth_uses_basic_x_access_token() {
        let header = github_git_auth_header("ghs_live_token_secret");
        assert!(
            header.starts_with("http.https://github.com/.extraHeader=Authorization: Basic "),
            "{header}"
        );
        assert!(!header.contains("http.extraHeader="), "{header}");
        assert!(!header.to_ascii_lowercase().contains("bearer"));
        assert!(!header.contains("ghs_live_token_secret"));
        let b64 = header.rsplit(' ').next().expect("b64");
        let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).unwrap();
        assert_eq!(raw, b"x-access-token:ghs_live_token_secret");
    }

    #[tokio::test]
    async fn clone_bare_rejects_non_github_https() {
        let dest = tempfile::tempdir().unwrap();
        let clone = dest.path().join("c.git");
        let err = clone_bare("https://evil.example/acme/box.git", &clone, Some("ghs_x"))
            .await
            .unwrap_err();
        assert!(
            matches!(err, GitError::Command(ref s) if s == "unsafe url"),
            "{err}"
        );
        assert!(!clone.exists());
    }

    #[tokio::test]
    async fn command_timeout_is_timeout_error() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        cmd.kill_on_drop(true);
        cmd.stdin(Stdio::null());
        let started = std::time::Instant::now();
        let err = run(cmd, Duration::from_millis(120)).await.unwrap_err();
        assert!(matches!(err, GitError::Timeout), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "timeout must not wait out the child"
        );
    }

    #[tokio::test]
    async fn timeout_kills_process_group() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 | sleep 30"]);
        cmd.kill_on_drop(true);
        cmd.stdin(Stdio::null());
        let started = std::time::Instant::now();
        let err = run(cmd, Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, GitError::Timeout), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "helpers must not outlive the 60s clone cap"
        );
    }

    #[test]
    fn parse_merge_tree_skips_unsafe_paths() {
        let sample = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
100644 1111111111111111111111111111111111111111 1\t.git/config\n\
100644 2222222222222222222222222222222222222222 1\t../etc/passwd\n\
100644 3333333333333333333333333333333333333333 1\t/abs.rs\n\
100644 4444444444444444444444444444444444444444 1\tsrc/lib.rs\n\
\n";
        let (_, paths) = parse_merge_tree_output(sample).unwrap();
        assert!(paths.contains("src/lib.rs"));
        assert!(!paths.contains(".git/config"));
        assert!(!paths.contains("../etc/passwd"));
        assert!(!paths.contains("/abs.rs"));
        assert_eq!(paths.len(), 1);
    }
}
