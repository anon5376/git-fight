use git_fight_server::sig;
use git_fight_server::{gh::GitHub, Auth, Config};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET: &[u8] = b"webhook-secret-for-tests";
const APP_PEM: &str = include_str!("fixtures/app_key.txt");

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

fn conflict_two_authors() -> (tempfile::TempDir, PathBuf, String, String, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("a.rs"), "fn a() { 1 }\n").unwrap();
    std::fs::write(work.join("b.rs"), "fn b() { 1 }\n").unwrap();
    git(&work, &["add", "a.rs", "b.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(work.join("a.rs"), "fn a() { 2 }\n").unwrap();
    std::fs::write(work.join("b.rs"), "fn b() { 2 }\n").unwrap();
    git(&work, &["add", "a.rs", "b.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("a.rs"), "fn a() { 3 }\n").unwrap();
    git(&work, &["add", "a.rs"]);
    git(&work, &["commit", "-q", "-m", "bob"]);
    let bob_sha = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["config", "user.email", "carol@example.com"]);
    git(&work, &["config", "user.name", "carol"]);
    std::fs::write(work.join("b.rs"), "fn b() { 3 }\n").unwrap();
    git(&work, &["add", "b.rs"]);
    git(&work, &["commit", "-q", "-m", "carol"]);
    let carol_sha = git(&work, &["rev-parse", "HEAD"]);
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
    (tmp, bare, head, carol_sha.clone(), bob_sha, carol_sha)
}

async fn spawn(cfg: Config) -> std::net::SocketAddr {
    spawn_with_pool(cfg).await.0
}

async fn spawn_with_pool(cfg: Config) -> (std::net::SocketAddr, sqlx::SqlitePool) {
    let dir = std::env::temp_dir().join(format!(
        "gf-wh-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
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
    (addr, pool)
}

async fn http(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>) {
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    s.write_all(body).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let rest = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, rest.as_bytes().to_vec())
}

fn fight_body() -> Vec<u8> {
    serde_json::to_vec(&json!({
        "action": "created",
        "installation": { "id": 1 },
        "repository": {
            "name": "box",
            "owner": { "login": "acme" },
            "default_branch": "main"
        },
        "issue": { "number": 1, "pull_request": {} },
        "comment": { "body": "/fight\n", "user": { "login": "carol", "type": "User" } },
        "sender": { "login": "carol", "type": "User" }
    }))
    .unwrap()
}

fn pr_opened_body() -> Vec<u8> {
    pr_event_body("opened", "abc", "def")
}

fn pr_event_body(action: &str, head: &str, base: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "action": action,
        "installation": { "id": 1 },
        "repository": {
            "name": "box",
            "owner": { "login": "acme" },
            "default_branch": "main"
        },
        "pull_request": {
            "number": 1,
            "head": { "sha": head },
            "base": { "sha": base }
        }
    }))
    .unwrap()
}

fn posted_comments(rec: &[wiremock::Request]) -> Vec<String> {
    rec.iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path().ends_with("/comments"))
        .filter_map(|r| {
            let posted: Value = serde_json::from_slice(&r.body).ok()?;
            posted["body"].as_str().map(str::to_string)
        })
        .collect()
}

fn patched_comments(rec: &[wiremock::Request]) -> Vec<String> {
    rec.iter()
        .filter(|r| r.method.as_str() == "PATCH" && r.url.path().contains("/issues/comments/"))
        .filter_map(|r| {
            let posted: Value = serde_json::from_slice(&r.body).ok()?;
            posted["body"].as_str().map(str::to_string)
        })
        .collect()
}

async fn wait_until(
    mock: &MockServer,
    min: usize,
    pick: fn(&[wiremock::Request]) -> Vec<String>,
) -> Vec<String> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let rec = mock.received_requests().await.unwrap_or_default();
        let got = pick(&rec);
        if got.len() >= min {
            return got;
        }
        if tokio::time::Instant::now() >= deadline {
            return got;
        }
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
    }
}

async fn wait_posted(mock: &MockServer, min: usize) -> Vec<String> {
    wait_until(mock, min, posted_comments).await
}

async fn wait_patched(mock: &MockServer, min: usize) -> Vec<String> {
    wait_until(mock, min, patched_comments).await
}

async fn settle_posted(mock: &MockServer) -> Vec<String> {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    posted_comments(&mock.received_requests().await.unwrap_or_default())
}

async fn settle_patched(mock: &MockServer) -> Vec<String> {
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    patched_comments(&mock.received_requests().await.unwrap_or_default())
}

async fn wait_challenge_comment_id(pool: &sqlx::SqlitePool, match_id: &str) -> Option<i64> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        if let Ok(Some(row)) = git_fight_server::db::get_match(pool, match_id).await {
            if row.challenge_comment_id.is_some() {
                return row.challenge_comment_id;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return match git_fight_server::db::get_match(pool, match_id).await {
                Ok(Some(row)) => row.challenge_comment_id,
                _ => None,
            };
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

fn match_id_from(comments: &[String]) -> String {
    comments
        .iter()
        .find_map(|t| {
            t.split("/match/")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(str::to_string)
        })
        .expect("challenge comment with /match/")
}

struct MockOpts {
    commit_author: Value,
    size: u64,
    auto_challenge: bool,
    commit_authors: Vec<(String, Value)>,
}

async fn github_mocks(head: &str, base: &str, opts: MockOpts) -> MockServer {
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
        .and(path("/repos/acme/box"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "size": opts.size,
            "default_branch": "main"
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
    if opts.auto_challenge {
        Mock::given(method("GET"))
            .and(path("/repos/acme/box/contents/.github/git-fight.yml"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "encoding": "utf-8",
                "content": "auto_challenge: true\n"
            })))
            .mount(&mock)
            .await;
    } else {
        Mock::given(method("GET"))
            .and(path_regex(r"/repos/acme/box/contents/.*"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&mock)
            .await;
    }
    if opts.commit_authors.is_empty() {
        Mock::given(method("GET"))
            .and(path_regex(r"/repos/acme/box/commits/.*"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "author": opts.commit_author
            })))
            .mount(&mock)
            .await;
    } else {
        for (sha, author) in &opts.commit_authors {
            Mock::given(method("GET"))
                .and(path(format!("/repos/acme/box/commits/{sha}")))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "author": author
                })))
                .mount(&mock)
                .await;
        }
    }
    Mock::given(method("POST"))
        .and(path("/repos/acme/box/issues/1/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": 99 })))
        .mount(&mock)
        .await;
    Mock::given(method("PATCH"))
        .and(path_regex(r"/repos/acme/box/issues/comments/\d+"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": 99 })))
        .mount(&mock)
        .await;
    mock
}

fn cpu_opts() -> MockOpts {
    MockOpts {
        commit_author: Value::Null,
        size: 12,
        auto_challenge: false,
        commit_authors: vec![],
    }
}

fn cfg_for(mock: &MockServer, bare: PathBuf) -> Config {
    let mut test_repos = HashMap::new();
    test_repos.insert("acme/box".into(), bare);
    Config {
        webhook_secret: Some(SECRET.to_vec()),
        github: Some(GitHub::new(
            mock.uri(),
            mock.uri(),
            1,
            APP_PEM.to_string(),
            "cid".into(),
            "csec".into(),
        )),
        auth: Auth {
            session_key: b"session-key-session-key-session!".to_vec(),
            public_url: "http://fight.test".into(),
        },
        test_repos,
        ..Config::default()
    }
}

async fn post_signed(addr: std::net::SocketAddr, event: &str, delivery: &str, body: &[u8]) -> u16 {
    let sig = sig::signature_header(SECRET, body);
    http(
        addr,
        "POST",
        "/webhooks/github",
        &[
            ("Content-Type", "application/json"),
            ("X-Hub-Signature-256", &sig),
            ("X-GitHub-Event", event),
            ("X-GitHub-Delivery", delivery),
        ],
        body,
    )
    .await
    .0
}

#[tokio::test]
async fn missing_signature_is_401() {
    let addr = spawn(Config {
        webhook_secret: Some(SECRET.to_vec()),
        ..Config::default()
    })
    .await;
    let body = b"{\"action\":\"created\"}";
    let (status, _) = http(
        addr,
        "POST",
        "/webhooks/github",
        &[("Content-Type", "application/json")],
        body,
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn invalid_signature_is_401() {
    let addr = spawn(Config {
        webhook_secret: Some(SECRET.to_vec()),
        ..Config::default()
    })
    .await;
    let body = b"{\"action\":\"created\"}";
    let (status, _) = http(
        addr,
        "POST",
        "/webhooks/github",
        &[
            ("Content-Type", "application/json"),
            (
                "X-Hub-Signature-256",
                "sha256=0000000000000000000000000000000000000000000000000000000000000000",
            ),
        ],
        body,
    )
    .await;
    assert_eq!(status, 401);
}

#[tokio::test]
async fn valid_fight_comments_challenge() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    let status = post_signed(addr, "issue_comment", "deliv-1", &fight_body()).await;
    assert_eq!(status, 200);
    let comments = wait_posted(&mock, 1).await;
    assert!(
        !comments.is_empty(),
        "expected a PR comment, got {comments:?}"
    );
    let text = &comments[0];
    assert!(text.contains("git fight"), "{text}");
    assert!(text.contains("/match/"), "{text}");
    assert!(text.contains("CPU"), "{text}");
    let id = match_id_from(&comments);
    assert_eq!(wait_challenge_comment_id(&pool, &id).await, Some(99));
}

#[tokio::test]
async fn same_login_is_a_mirror_match() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(
        &head,
        &base,
        MockOpts {
            commit_author: json!({ "login": "alice" }),
            size: 12,
            auto_challenge: false,
            commit_authors: vec![],
        },
    )
    .await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let status = post_signed(addr, "issue_comment", "deliv-mirror", &fight_body()).await;
    assert_eq!(status, 200);
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("mirror")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn oversized_repo_is_skipped() {
    let mock = github_mocks(
        "dead",
        "beef",
        MockOpts {
            commit_author: Value::Null,
            size: 1_048_577,
            auto_challenge: false,
            commit_authors: vec![],
        },
    )
    .await;
    let addr = spawn(cfg_for(&mock, PathBuf::from("/nope"))).await;
    let status = post_signed(addr, "issue_comment", "deliv-big", &fight_body()).await;
    assert_eq!(status, 200);
    let comments = wait_posted(&mock, 1).await;
    assert!(comments.iter().any(|t| t.contains("1 GB")), "{comments:?}");
}

#[tokio::test]
async fn bot_fight_is_ignored() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let body = serde_json::to_vec(&json!({
        "action": "created",
        "installation": { "id": 1 },
        "repository": {
            "name": "box",
            "owner": { "login": "acme" },
            "default_branch": "main"
        },
        "issue": { "number": 1, "pull_request": {} },
        "comment": { "body": "/fight\n", "user": { "login": "git-fight[bot]", "type": "Bot" } },
        "sender": { "login": "git-fight[bot]", "type": "Bot" }
    }))
    .unwrap();
    let status = post_signed(addr, "issue_comment", "deliv-bot", &body).await;
    assert_eq!(status, 200);
    let comments = settle_posted(&mock).await;
    assert!(comments.is_empty(), "{comments:?}");
}

#[tokio::test]
async fn auto_challenge_stays_off_without_yaml() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let status = post_signed(addr, "pull_request", "deliv-pr", &pr_opened_body()).await;
    assert_eq!(status, 200);
    let comments = settle_posted(&mock).await;
    assert!(comments.is_empty(), "{comments:?}");
}

#[tokio::test]
async fn auto_challenge_starts_when_yaml_set() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(
        &head,
        &base,
        MockOpts {
            commit_author: Value::Null,
            size: 12,
            auto_challenge: true,
            commit_authors: vec![],
        },
    )
    .await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let status = post_signed(addr, "pull_request", "deliv-auto", &pr_opened_body()).await;
    assert_eq!(status, 200);
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("/match/")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn blame_email_maps_through_commits_api() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/commits"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([{
            "author": { "login": "bob" },
            "commit": { "author": { "email": "bob@example.com" } }
        }])))
        .mount(&mock)
        .await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let status = post_signed(addr, "issue_comment", "deliv-email", &fight_body()).await;
    assert_eq!(status, 200);
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("alice vs bob") && !t.contains("CPU")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn pr_synchronize_same_sha_does_not_comment() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-fight", &fight_body()).await,
        200
    );
    let after_fight = wait_posted(&mock, 1).await;
    assert_eq!(after_fight.len(), 1, "expected one challenge comment");
    assert_eq!(
        post_signed(
            addr,
            "pull_request",
            "deliv-sync-same",
            &pr_event_body("synchronize", &head, &base)
        )
        .await,
        200
    );
    let comments = settle_posted(&mock).await;
    assert_eq!(comments.len(), 1, "{comments:?}");
}

#[tokio::test]
async fn pr_synchronize_moved_sha_comments_once() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-fight2", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert_eq!(comments.len(), 1, "challenge stays one POST: {comments:?}");
    let id = match_id_from(&comments);
    assert_eq!(wait_challenge_comment_id(&pool, &id).await, Some(99));
    assert_eq!(
        post_signed(
            addr,
            "pull_request",
            "deliv-sync-move",
            &pr_event_body(
                "synchronize",
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &base
            )
        )
        .await,
        200
    );
    let patched = wait_patched(&mock, 1).await;
    assert_eq!(patched.len(), 1, "{patched:?}");
    assert!(
        patched[0].contains("outdated") && patched[0].contains("/fight"),
        "{patched:?}"
    );
    assert_eq!(
        post_signed(
            addr,
            "pull_request",
            "deliv-sync-move-2",
            &pr_event_body(
                "synchronize",
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &base
            )
        )
        .await,
        200
    );
    let comments = settle_posted(&mock).await;
    let patched = settle_patched(&mock).await;
    assert_eq!(
        comments.len(),
        1,
        "outdated notice must not post again: {comments:?}"
    );
    assert_eq!(
        patched.len(),
        1,
        "outdated notice must not edit again: {patched:?}"
    );
}

#[tokio::test]
async fn installation_rate_limit_skips_clone() {
    use git_fight_server::db::NewMatch;
    use git_fight_server::MAX_MATCHES_PER_INSTALL_HOUR;
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    for i in 0..MAX_MATCHES_PER_INSTALL_HOUR {
        git_fight_server::db::insert_full_match(
            &pool,
            &NewMatch {
                id: format!("rate{i:024}"),
                seed: i as u64,
                delay: 3,
                ours_name: "alice".into(),
                theirs_name: "bob".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: Some("alice".into()),
                theirs_login: None,
                ours_token: format!("o{i}"),
                theirs_token: format!("t{i}"),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "other".into(),
                pr_number: 100 + i,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
    }
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-rate", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("too many fights")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn pr_rate_limit_skips_clone() {
    use git_fight_server::db::NewMatch;
    use git_fight_server::MAX_MATCHES_PER_PR_HOUR;
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    for i in 0..MAX_MATCHES_PER_PR_HOUR {
        let id = format!("prr{i:025}");
        git_fight_server::db::insert_full_match(
            &pool,
            &NewMatch {
                id: id.clone(),
                seed: i as u64,
                delay: 3,
                ours_name: "alice".into(),
                theirs_name: "bob".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: Some("alice".into()),
                theirs_login: None,
                ours_token: format!("o{i}"),
                theirs_token: format!("t{i}"),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        git_fight_server::db::set_status(&pool, &id, "finished", true, true, None, None)
            .await
            .unwrap();
    }
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-pr-rate", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("too many fights on this pull request")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn each_hunk_stores_blamed_author_login() {
    let (_keep, bare, head, base, bob_sha, carol_sha) = conflict_two_authors();
    let mock = github_mocks(
        &head,
        &base,
        MockOpts {
            commit_author: Value::Null,
            size: 12,
            auto_challenge: false,
            commit_authors: vec![
                (bob_sha, json!({ "login": "bob" })),
                (carol_sha, json!({ "login": "carol" })),
            ],
        },
    )
    .await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-hunks", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    let text = comments
        .iter()
        .find(|t| t.contains("/match/"))
        .expect("challenge comment");
    let id = text
        .split("/match/")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();
    let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert_eq!(
        hunks.len(),
        2,
        "{:?}",
        hunks.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    let logins: Vec<Option<String>> = hunks.into_iter().map(|h| h.theirs_login).collect();
    assert!(logins.contains(&Some("bob".into())), "{logins:?}");
    assert!(logins.contains(&Some("carol".into())), "{logins:?}");
}
