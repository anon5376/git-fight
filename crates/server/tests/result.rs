use futures_util::{SinkExt, StreamExt};
use git_fight_server::db::{NewHunk, NewMatch};
use git_fight_server::gitutil;
use git_fight_server::{gh::GitHub, Auth, Config, ResultCtx};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const APP_PEM: &str = include_str!("fixtures/app_key.txt");
const KEY: &[u8] = b"session-key-session-key-session!";
const MATCH_ID: &str = "cafe0001cafe0001cafe0001cafe0001";

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

fn git_dir(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "core.hooksPath=/dev/null"])
        .arg("--git-dir")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("git");
    if !out.status.success() {
        panic!(
            "git --git-dir {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
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

fn two_hunk_conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("lib.rs"), "fn a() { 0 }\nfn b() { 0 }\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(work.join("lib.rs"), "fn a() { 1 }\nfn b() { 3 }\n").unwrap();
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("lib.rs"), "fn a() { 2 }\nfn b() { 4 }\n").unwrap();
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

async fn github_mocks(head: &str, base: &str) -> MockServer {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/app/installations/1/access_tokens"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "token": "ghs_test_token",
            "expires_at": "2099-01-01T00:00:00Z"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/pulls/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "number": 1,
            "mergeable": false,
            "head": { "sha": head, "ref": "pr" },
            "base": { "sha": base, "ref": "main" },
            "user": { "login": "alice" }
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/acme/box/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 42 })))
        .mount(&mock)
        .await;
    Mock::given(method("PATCH"))
        .and(path("/repos/acme/box/issues/comments/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 42 })))
        .mount(&mock)
        .await;
    mock
}

fn gh(mock: &MockServer) -> GitHub {
    GitHub::new(
        mock.uri(),
        mock.uri(),
        1,
        APP_PEM.to_string(),
        "cid".into(),
        "csec".into(),
    )
}

async fn pool() -> sqlx::SqlitePool {
    git_fight_server::db_connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn seed_match(
    pool: &sqlx::SqlitePool,
    head: &str,
    base: &str,
    winner: Option<&str>,
) -> String {
    git_fight_server::db::insert_full_match(
        pool,
        &NewMatch {
            id: MATCH_ID.into(),
            seed: 1,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: Some("alice".into()),
            theirs_login: None,
            ours_token: "o".into(),
            theirs_token: "t".into(),
            expire_secs: 3600,
            installation_id: Some(1),
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: head.into(),
            pr_base_sha: base.into(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        pool,
        &NewHunk {
            match_id: MATCH_ID,
            round: 0,
            path: "lib.rs",
            hunk_index: 0,
            ours: b"fn v() { 2 }\n",
            theirs: b"fn v() { 3 }\n",
            base: b"fn v() { 1 }\n",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: Default::default(),
            theirs_stats: Default::default(),
        },
    )
    .await
    .unwrap();
    if let Some(w) = winner {
        git_fight_server::db::set_hunk_winner(pool, MATCH_ID, 0, w)
            .await
            .unwrap();
    }
    MATCH_ID.to_string()
}

fn ctx(pool: sqlx::SqlitePool, mock: &MockServer, bare: PathBuf) -> ResultCtx {
    let mut test_repos = HashMap::new();
    test_repos.insert("acme/box".into(), bare);
    ResultCtx {
        gh: Some(gh(mock)),
        pool,
        public_url: "http://fight.test".into(),
        test_repos,
    }
}

async fn posted_comments(mock: &MockServer) -> Vec<String> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/comments"))
        .filter_map(|r| {
            let posted: Value = serde_json::from_slice(&r.body).ok()?;
            posted["body"].as_str().map(str::to_string)
        })
        .collect()
}

async fn patched_comments(mock: &MockServer) -> Vec<String> {
    mock.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PATCH" && r.url.path().contains("/issues/comments/"))
        .filter_map(|r| {
            let posted: Value = serde_json::from_slice(&r.body).ok()?;
            posted["body"].as_str().map(str::to_string)
        })
        .collect()
}

fn heads(bare: &Path) -> Vec<String> {
    let out = Command::new("git")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .args(["-c", "core.hooksPath=/dev/null"])
        .arg("--git-dir")
        .arg(bare)
        .args(["show-ref", "--heads"])
        .output()
        .expect("show-ref");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_whitespace().nth(1).map(str::to_string))
        .collect()
}

#[tokio::test]
async fn ours_win_pushes_new_git_fight_branch() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("ours")).await;
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();

    let branch = format!("git-fight/pr-1-{MATCH_ID}");
    assert!(
        heads(&bare)
            .iter()
            .any(|r| r == &format!("refs/heads/{branch}")),
        "missing {branch} in {:?}",
        heads(&bare)
    );
    let blob = git_dir(&bare, &["show", &format!("{branch}:lib.rs")]);
    assert_eq!(blob, "fn v() { 2 }");
    let parents = git_dir(&bare, &["rev-list", "--parents", "-n1", &branch]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "{parents}");
    assert!(parts.contains(&head.as_str()));
    assert!(parts.contains(&base.as_str()));
    let msg = git_dir(&bare, &["log", "-1", "--format=%s%n%b", &branch]);
    assert!(msg.contains("round 1"), "{msg}");
    assert!(msg.contains("ours"), "{msg}");
    let comments = posted_comments(&mock).await;
    assert!(
        comments
            .iter()
            .any(|c| c.contains(&branch) && c.contains("compare:") && c.contains("/replay/")),
        "{comments:?}"
    );
    let row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.result_branch.as_deref(), Some(branch.as_str()));
    assert!(row.abort_reason.is_none());
}

#[tokio::test]
async fn theirs_win_keeps_base_side() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("theirs")).await;
    let ctx = ctx(pool, &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    let branch = format!("git-fight/pr-1-{MATCH_ID}");
    let blob = git_dir(&bare, &["show", &format!("{branch}:lib.rs")]);
    assert_eq!(blob, "fn v() { 3 }");
}

#[tokio::test]
async fn two_hunks_in_one_file_push_each_pick() {
    use git_fight_core::{ConflictFile, Pick};
    const ID: &str = "cafe0002cafe0002cafe0002cafe0002";
    let (_keep, bare, head, base) = two_hunk_conflict_bare();
    let work = tempfile::tempdir().unwrap();
    let clone = work.path().join("c.git");
    let url = format!("file://{}", bare.display());
    gitutil::clone_bare(&url, &clone, None).await.unwrap();
    let (tree, paths, code) = gitutil::merge_tree(&clone, &base, &head).await.unwrap();
    assert_eq!(code, 1);
    let collected = gitutil::collect_hunks(&clone, &tree, &base, &paths)
        .await
        .unwrap();
    assert_eq!(collected.len(), 2, "expected two fightable hunks in lib.rs");
    assert!(collected.iter().all(|h| h.path == "lib.rs"));

    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: ID.into(),
            seed: 1,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: Some("alice".into()),
            theirs_login: None,
            ours_token: "o".into(),
            theirs_token: "t".into(),
            expire_secs: 3600,
            installation_id: Some(1),
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: head.clone(),
            pr_base_sha: base.clone(),
        },
    )
    .await
    .unwrap();
    for (round, h) in collected.iter().enumerate() {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: ID,
                round: round as i64,
                path: &h.path,
                hunk_index: h.hunk_index as i64,
                ours: &h.ours,
                theirs: &h.theirs,
                base: &h.base,
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: Default::default(),
                theirs_stats: Default::default(),
            },
        )
        .await
        .unwrap();
        let winner = if round == 0 { "ours" } else { "theirs" };
        git_fight_server::db::set_hunk_winner(&pool, ID, round as i64, winner)
            .await
            .unwrap();
    }
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, ID).await.unwrap();

    let branch = format!("git-fight/pr-1-{ID}");
    assert!(
        heads(&bare)
            .iter()
            .any(|r| r == &format!("refs/heads/{branch}")),
        "missing {branch} in {:?}",
        heads(&bare)
    );
    let merge_blob = gitutil::cat_blob(&clone, &format!("{tree}:lib.rs"))
        .await
        .unwrap();
    let parsed = ConflictFile::parse(&merge_blob).unwrap();
    assert_eq!(parsed.hunk_count(), 2);
    let expected = parsed.resolve(&[Some(Pick::Theirs), Some(Pick::Ours)]);
    let blob = git_dir(&bare, &["show", &format!("{branch}:lib.rs")]);
    assert_eq!(blob, String::from_utf8_lossy(&expected).trim(), "{blob}");
    let msg = git_dir(&bare, &["log", "-1", "--format=%s%n%b", &branch]);
    assert!(msg.contains("round 1"), "{msg}");
    assert!(msg.contains("round 2"), "{msg}");
    let row = git_fight_server::db::get_match(&pool, ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.result_branch.as_deref(), Some(branch.as_str()));
}

#[tokio::test]
async fn draw_skips_push() {
    let (_keep, bare, head, base) = conflict_bare();
    let before = heads(&bare);
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("draw")).await;
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    assert_eq!(heads(&bare), before);
    let comments = posted_comments(&mock).await;
    assert!(
        comments
            .iter()
            .any(|c| c.contains("nothing pushed") && c.contains("draw")),
        "{comments:?}"
    );
    let row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(row.result_branch.is_none());
    assert_eq!(row.abort_reason.as_deref(), Some("draw"));
}

#[test]
fn forfeit_is_not_a_git_side_pick() {
    use git_fight_core::Pick;
    use git_fight_server::result::git_pick_for_winner;
    assert_eq!(git_pick_for_winner("ours"), Some(Pick::Theirs));
    assert_eq!(git_pick_for_winner("theirs"), Some(Pick::Ours));
    assert_eq!(git_pick_for_winner("draw"), None);
    assert_eq!(git_pick_for_winner("forfeit_ours"), None);
    assert_eq!(git_pick_for_winner("forfeit_theirs"), None);
}

#[tokio::test]
async fn forfeit_skips_push() {
    let (_keep, bare, head, base) = conflict_bare();
    let before = heads(&bare);
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("forfeit_ours")).await;
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    assert_eq!(heads(&bare), before);
    let comments = posted_comments(&mock).await;
    assert!(
        comments.iter().any(|c| {
            c.contains("nothing pushed") && c.contains("forfeit") && c.contains("lib.rs")
        }),
        "{comments:?}"
    );
    let row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(row.result_branch.is_none());
    assert_eq!(row.abort_reason.as_deref(), Some("forfeit"));
}

#[tokio::test]
async fn forfeit_skips_push_even_when_another_round_was_won() {
    let (_keep, bare, head, base) = conflict_bare();
    let before = heads(&bare);
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("ours")).await;
    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: MATCH_ID,
            round: 1,
            path: "lib.rs",
            hunk_index: 1,
            ours: b"fn v() { 2 }\n",
            theirs: b"fn v() { 3 }\n",
            base: b"fn v() { 1 }\n",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: Default::default(),
            theirs_stats: Default::default(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::set_hunk_winner(&pool, MATCH_ID, 1, "forfeit_theirs")
        .await
        .unwrap();
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    assert_eq!(heads(&bare), before);
    let comments = posted_comments(&mock).await;
    assert!(
        comments
            .iter()
            .any(|c| c.contains("nothing pushed") && c.contains("forfeit_theirs")),
        "{comments:?}"
    );
    let row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    assert!(row.result_branch.is_none());
    assert_eq!(row.abort_reason.as_deref(), Some("forfeit"));
}

#[tokio::test]
async fn outdated_pr_skips_push() {
    let (_keep, bare, head, base) = conflict_bare();
    let before = heads(&bare);
    let mock = github_mocks("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("ours")).await;
    let ctx = ctx(pool.clone(), &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    assert_eq!(heads(&bare), before);
    let comments = posted_comments(&mock).await;
    assert!(
        comments
            .iter()
            .any(|c| c.contains("outdated") && c.contains("/fight")),
        "{comments:?}"
    );
    let row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.abort_reason.as_deref(), Some("outdated"));
}

#[tokio::test]
async fn expired_pending_match_comments_and_skips_push() {
    let (_keep, bare, head, base) = conflict_bare();
    let before = heads(&bare);
    let mock = github_mocks(&head, &base).await;
    let dir = std::env::temp_dir().join(format!(
        "gf-expire-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    seed_match(&pool, &head, &base, None).await;
    git_fight_server::db::set_challenge_comment_id(&pool, MATCH_ID, 42)
        .await
        .unwrap();
    sqlx::query("UPDATE matches SET expires_at = '2000-01-01T00:00:00+00:00' WHERE id = ?")
        .bind(MATCH_ID)
        .execute(&pool)
        .await
        .unwrap();
    let mut test_repos = HashMap::new();
    test_repos.insert("acme/box".into(), bare.clone());
    let cfg = Config {
        github: Some(gh(&mock)),
        auth: Auth {
            session_key: KEY.to_vec(),
            public_url: "http://fight.test".into(),
        },
        test_repos,
        ..Config::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(listener, serve_pool, cfg)
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let mut row = git_fight_server::db::get_match(&pool, MATCH_ID)
        .await
        .unwrap()
        .unwrap();
    for _ in 0..80 {
        if row.status == "expired" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        row = git_fight_server::db::get_match(&pool, MATCH_ID)
            .await
            .unwrap()
            .unwrap();
    }
    assert_eq!(row.status, "expired");
    assert_eq!(row.abort_reason.as_deref(), Some("expired"));
    assert!(row.result_branch.is_none());
    assert!(row.final_hash.is_none());
    assert_eq!(heads(&bare), before);
    let patched = patched_comments(&mock).await;
    assert!(
        patched.iter().any(|c| {
            c.contains("expired") && c.contains("Nothing was pushed") && c.contains("/fight")
        }),
        "patched={patched:?} posted={:?}",
        posted_comments(&mock).await
    );
}

#[tokio::test]
async fn result_edits_challenge_comment_when_id_set() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("ours")).await;
    git_fight_server::db::set_challenge_comment_id(&pool, MATCH_ID, 42)
        .await
        .unwrap();
    let ctx = ctx(pool, &mock, bare.clone());
    git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap();
    let patched = patched_comments(&mock).await;
    assert!(
        patched
            .iter()
            .any(|c| c.contains("git fight finished") && c.contains("/replay/")),
        "{patched:?}"
    );
    assert!(
        posted_comments(&mock).await.is_empty(),
        "should edit the challenge comment instead of posting another"
    );
}

#[tokio::test]
async fn create_only_does_not_overwrite_existing_ref() {
    let (_keep, bare, head, base) = conflict_bare();
    let branch = format!("git-fight/pr-1-{MATCH_ID}");
    git_dir(
        &bare,
        &["update-ref", &format!("refs/heads/{branch}"), &head],
    );
    let before = git_dir(&bare, &["rev-parse", &branch]);
    let mock = github_mocks(&head, &base).await;
    let pool = pool().await;
    seed_match(&pool, &head, &base, Some("ours")).await;
    let ctx = ctx(pool, &mock, bare.clone());
    let err = git_fight_server::publish_result(&ctx, MATCH_ID)
        .await
        .unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    let after = git_dir(&bare, &["rev-parse", &branch]);
    assert_eq!(before, after);
}

#[tokio::test]
async fn push_create_only_never_uses_force() {
    let src =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/gitutil.rs")).unwrap();
    assert!(
        !src.contains("--force"),
        "gitutil must never pass --force to git push"
    );
}

#[tokio::test]
async fn plumbing_hash_object_write_tree_commit() {
    let (_keep, bare, head, base) = conflict_bare();
    let dest = tempfile::tempdir().unwrap();
    let clone = dest.path().join("c.git");
    let url = format!("file://{}", bare.display());
    gitutil::clone_bare(&url, &clone, None).await.unwrap();
    let (tree, ..) = gitutil::merge_tree(&clone, &base, &head).await.unwrap();
    let resolved = gitutil::build_resolved_tree(
        &clone,
        &tree,
        &[("lib.rs".into(), b"fn v() { 2 }\n".to_vec())],
    )
    .await
    .unwrap();
    let commit = gitutil::commit_tree(&clone, &resolved, &[&head, &base], "git fight match x\n")
        .await
        .unwrap();
    gitutil::push_create_only(&clone, &url, &commit, "git-fight/pr-9-abc", None)
        .await
        .unwrap();
    assert_eq!(
        git_dir(&bare, &["show", "git-fight/pr-9-abc:lib.rs"]),
        "fn v() { 2 }"
    );
}

#[tokio::test]
async fn live_match_vs_cpu_pushes_after_last_round() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base).await;
    let dir = std::env::temp_dir().join(format!(
        "gf-res-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    seed_match(&pool, &head, &base, None).await;
    let mut test_repos = HashMap::new();
    test_repos.insert("acme/box".into(), bare.clone());
    let cfg = Config {
        instant: true,
        github: Some(gh(&mock)),
        auth: Auth {
            session_key: KEY.to_vec(),
            public_url: "http://fight.test".into(),
        },
        test_repos,
        ..Config::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(listener, serve_pool, cfg)
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    let cookie = git_fight_server::sign_session(KEY, "sid-alice");
    let url = format!("ws://{addr}/ws?match={MATCH_ID}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Cookie", format!("git_fight_sid={cookie}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let mut next_send = 0u32;
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timeout")
            .expect("closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("hello") => {
                assert_eq!(v["your_role"].as_str(), Some("ours"));
                while next_send < 16 {
                    let punch = if next_send.is_multiple_of(8) { 1 } else { 0 };
                    let body =
                        format!(r#"{{"type":"input","tick":{next_send},"buttons":{punch}}}"#);
                    sink.send(Message::Text(body.into())).await.unwrap();
                    next_send += 1;
                }
            }
            Some("tick") => {
                let n = v["n"].as_u64().unwrap() as u32;
                while next_send <= n + 8 {
                    let punch = if next_send.is_multiple_of(8) { 1 } else { 0 };
                    let body =
                        format!(r#"{{"type":"input","tick":{next_send},"buttons":{punch}}}"#);
                    sink.send(Message::Text(body.into())).await.unwrap();
                    next_send += 1;
                }
            }
            Some("end") => {
                assert_eq!(v["match_over"].as_bool(), Some(true));
                break;
            }
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }
    let mut branch = None;
    for _ in 0..50 {
        let row = git_fight_server::db::get_match(&pool, MATCH_ID)
            .await
            .unwrap()
            .unwrap();
        if row.result_branch.is_some() || row.abort_reason.is_some() {
            branch = row.result_branch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let comments = posted_comments(&mock).await;
    assert!(
        branch.is_some()
            || comments
                .iter()
                .any(|c| c.contains("nothing pushed") || c.contains("finished")),
        "no result: branch={branch:?} comments={comments:?} heads={:?}",
        heads(&bare)
    );
    if let Some(b) = branch {
        assert!(b.starts_with("git-fight/pr-1-"), "{b}");
        assert!(heads(&bare).iter().any(|r| r.ends_with(&b)));
        assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/pr"]), head);
        assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/base"]), base);
    }
}

#[tokio::test]
async fn local_match_does_not_push() {
    let (_keep, bare, _head, _base) = conflict_bare();
    let before = heads(&bare);
    let pool = pool().await;
    git_fight_server::db::insert_match(&pool, "aabbccdd", 1, 3, "o", "t", 3600)
        .await
        .unwrap();
    let ctx = ResultCtx {
        gh: None,
        pool,
        public_url: "http://fight.test".into(),
        test_repos: HashMap::new(),
    };
    git_fight_server::publish_result(&ctx, "aabbccdd")
        .await
        .unwrap();
    assert_eq!(heads(&bare), before);
}
