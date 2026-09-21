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

#[tokio::test]
async fn fighter_stats_match_cli_formula() {
    let (_keep, bare, head, base) = conflict_bare();
    let dest = tempfile::tempdir().unwrap();
    let clone = dest.path().join("c.git");
    let url = format!("file://{}", bare.display());
    gitutil::clone_bare(&url, &clone, None).await.unwrap();
    let _ = gitutil::fetch_shas(&clone, &[&head, &base], None).await;
    let ours = gitutil::fighter_stats(&clone, &head, "lib.rs", "alice").await;
    let theirs = gitutil::fighter_stats(&clone, &base, "lib.rs", "bob").await;
    assert_eq!(ours.hp, 120, "{ours:?}");
    assert!(!ours.armor, "{ours:?}");
    assert!(!ours.special, "{ours:?}");
    assert_eq!(theirs.hp, 120, "{theirs:?}");
    assert!(!theirs.armor);
    assert!(!theirs.special);
    assert_eq!(
        gitutil::latest_author(&clone, &head, "lib.rs")
            .await
            .as_deref(),
        Some("alice")
    );
    assert_eq!(
        gitutil::latest_author(&clone, &base, "lib.rs")
            .await
            .as_deref(),
        Some("bob")
    );
}

fn git_dated(cwd: &Path, date: &str, args: &[&str]) {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .args(["-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .expect("git");
    if !out.status.success() {
        panic!("git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }
}

#[tokio::test]
async fn fighter_stats_armor_and_special() {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("lib.rs"), "fn v() { 1 }\n").unwrap();
    git_dated(&work, "2026-09-16T12:00:00 +0000", &["add", "lib.rs"]);
    git_dated(
        &work,
        "2026-09-16T12:00:00 +0000",
        &["commit", "-q", "-m", "base"],
    );
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::create_dir(work.join("tests")).unwrap();
    std::fs::write(work.join("lib.rs"), "fn v() { 2 }\n").unwrap();
    std::fs::write(work.join("tests/t.rs"), "ok\n").unwrap();
    git_dated(
        &work,
        "2026-09-18T12:00:00 +0000",
        &["add", "lib.rs", "tests/t.rs"],
    );
    git_dated(
        &work,
        "2026-09-18T12:00:00 +0000",
        &["commit", "-q", "-m", "pr+test"],
    );
    std::fs::write(work.join("notes.txt"), "d1\n").unwrap();
    git_dated(&work, "2026-09-19T12:00:00 +0000", &["add", "notes.txt"]);
    git_dated(
        &work,
        "2026-09-19T12:00:00 +0000",
        &["commit", "-q", "-m", "d2"],
    );
    std::fs::write(work.join("notes.txt"), "d2\n").unwrap();
    git_dated(&work, "2026-09-20T12:00:00 +0000", &["add", "notes.txt"]);
    git_dated(
        &work,
        "2026-09-20T12:00:00 +0000",
        &["commit", "-q", "-m", "d3"],
    );
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
    let dest = tempfile::tempdir().unwrap();
    let clone = dest.path().join("c.git");
    gitutil::clone_bare(&format!("file://{}", bare.display()), &clone, None)
        .await
        .unwrap();
    let _ = gitutil::fetch_shas(&clone, &[&head, &base], None).await;
    let ours = gitutil::fighter_stats(&clone, &head, "lib.rs", "alice").await;
    let theirs = gitutil::fighter_stats(&clone, &base, "lib.rs", "bob").await;
    assert_eq!(ours.hp, 120, "{ours:?}");
    assert!(ours.armor, "{ours:?}");
    assert!(ours.special, "{ours:?}");
    assert_eq!(theirs.hp, 120, "{theirs:?}");
    assert!(!theirs.armor, "{theirs:?}");
    assert!(!theirs.special, "{theirs:?}");
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

fn file_directory_conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
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
    git(&work, &["rm", "-q", "lib.rs"]);
    std::fs::create_dir(work.join("lib.rs")).unwrap();
    std::fs::write(work.join("lib.rs").join("mod.rs"), "mod inner;\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "pr-dir"]);
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
async fn file_directory_conflict_has_no_fightable_hunks() {
    let (_keep, bare, head, base) = file_directory_conflict_bare();
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
        gitutil::GitError::NothingToFight => {}
        other => panic!("expected NothingToFight, got {other}"),
    }
}

fn gitlink_conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("sub"), "mod\n").unwrap();
    git(&work, &["add", "sub"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    let target = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["rm", "-q", "sub"]);
    git(
        &work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{target},sub"),
        ],
    );
    git(&work, &["commit", "-q", "-m", "pr-gitlink"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("sub"), "mod2\n").unwrap();
    git(&work, &["add", "sub"]);
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
async fn gitlink_conflict_has_no_fightable_hunks() {
    let (_keep, bare, head, base) = gitlink_conflict_bare();
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
        gitutil::GitError::NothingToFight => {}
        other => panic!("expected NothingToFight, got {other}"),
    }
}

fn oversized_blob_bare() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("blob.rs"), vec![b'x'; 64]).unwrap();
    git(&work, &["add", "blob.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(work.join("blob.rs"), vec![b'a'; 1_048_577]).unwrap();
    git(&work, &["add", "blob.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("blob.rs"), vec![b'b'; 1_048_577]).unwrap();
    git(&work, &["add", "blob.rs"]);
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
async fn oversized_blob_has_no_fightable_hunks() {
    let (_keep, bare, head, base) = oversized_blob_bare();
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
        gitutil::GitError::NothingToFight => {}
        other => panic!("expected NothingToFight, got {other}"),
    }
}
