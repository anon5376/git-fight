use git_fight_server::gitutil;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn git(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("git");
    if !out.status.success() {
        panic!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

pub fn conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("lib.rs"), "fn v() { 1 }\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(work.join("lib.rs"), "fn v() { 2 }\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("lib.rs"), "fn v() { 3 }\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base2"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    let bare = tmp.path().join("repo.git");
    git(
        tmp.path(),
        &[
            "clone",
            "--bare",
            "--filter=blob:none",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    (tmp, bare, head, base)
}

#[tokio::test]
async fn merge_tree_finds_fightable_hunk() {
    let (_keep, bare, head, base) = conflict_bare();
    let dest = tempfile::tempdir().unwrap();
    let clone = dest.path().join("c.git");
    let url = format!("file://{}", bare.display());
    gitutil::clone_bare(&url, &clone, None).await.unwrap();
    let (tree, paths, code) = gitutil::merge_tree(&clone, &base, &head).await.unwrap();
    assert_eq!(code, 1);
    assert!(paths.contains("lib.rs"));
    let hunks = gitutil::collect_hunks(&clone, &tree, &base, &paths)
        .await
        .unwrap();
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0].path, "lib.rs");
    assert_eq!(hunks[0].ours, b"fn v() { 2 }\n");
    assert_eq!(hunks[0].theirs, b"fn v() { 3 }\n");
    assert_eq!(hunks[0].blame_email, "bob@example.com");
}

fn many_hunks_bare(n: usize) -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("fn f{i}() {{ 0 }}\n"));
        for p in 0..8 {
            src.push_str(&format!("// pad {i} {p}\n"));
        }
    }
    std::fs::write(work.join("lib.rs"), &src).unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("fn f{i}() {{ 1 }}\n"));
        for p in 0..8 {
            src.push_str(&format!("// pad {i} {p}\n"));
        }
    }
    std::fs::write(work.join("lib.rs"), &src).unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("fn f{i}() {{ 2 }}\n"));
        for p in 0..8 {
            src.push_str(&format!("// pad {i} {p}\n"));
        }
    }
    std::fs::write(work.join("lib.rs"), &src).unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base2"]);
    let base = git(&work, &["rev-parse", "HEAD"]);
    let bare = tmp.path().join("repo.git");
    git(
        tmp.path(),
        &[
            "clone",
            "--bare",
            "--filter=blob:none",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    (tmp, bare, head, base)
}

#[tokio::test]
async fn more_than_fifteen_hunks_is_too_many() {
    let (_keep, bare, head, base) = many_hunks_bare(16);
    let dest = tempfile::tempdir().unwrap();
    let clone = dest.path().join("c.git");
    let url = format!("file://{}", bare.display());
    gitutil::clone_bare(&url, &clone, None).await.unwrap();
    let (tree, paths, code) = gitutil::merge_tree(&clone, &base, &head).await.unwrap();
    assert_eq!(code, 1);
    let err = gitutil::collect_hunks(&clone, &tree, &base, &paths)
        .await
        .unwrap_err();
    match err {
        gitutil::GitError::TooMany(n) => assert!(n > 15, "{n}"),
        other => panic!("expected TooMany, got {other}"),
    }
}
