use futures_util::{SinkExt, StreamExt};
use git_fight_server::sig;
use git_fight_server::{gh::GitHub, Auth, Config};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const SECRET: &[u8] = b"webhook-secret-for-tests";
const APP_PEM: &str = include_str!("fixtures/app_key.txt");
const SESSION_KEY: &[u8] = b"session-key-session-key-session!";

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

fn binary_conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    std::fs::write(work.join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    git(&work, &["add", "blob.bin"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    std::fs::write(work.join("blob.bin"), [0u8, 9, 9, 9]).unwrap();
    git(&work, &["add", "blob.bin"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    std::fs::write(work.join("blob.bin"), [0u8, 7, 7, 7]).unwrap();
    git(&work, &["add", "blob.bin"]);
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

fn modify_delete_conflict_bare() -> (tempfile::TempDir, PathBuf, String, String) {
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
    git(&work, &["commit", "-q", "-m", "pr-delete"]);
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

fn write_hunk_fns(work: &Path, n: usize, body: i32) {
    let mut src = String::new();
    for i in 0..n {
        src.push_str(&format!("fn f{i}() {{ {body} }}\n"));
        for p in 0..8 {
            src.push_str(&format!("// pad {i} {p}\n"));
        }
    }
    std::fs::write(work.join("lib.rs"), src).unwrap();
}

fn many_hunks_bare(n: usize) -> (tempfile::TempDir, PathBuf, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let work = tmp.path().join("work");
    std::fs::create_dir(&work).unwrap();
    git(&work, &["init", "-q"]);
    git(&work, &["config", "user.email", "alice@example.com"]);
    git(&work, &["config", "user.name", "alice"]);
    write_hunk_fns(&work, n, 0);
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "base"]);
    git(&work, &["branch", "base"]);
    git(&work, &["checkout", "-q", "-b", "pr"]);
    write_hunk_fns(&work, n, 1);
    git(&work, &["add", "lib.rs"]);
    git(&work, &["commit", "-q", "-m", "pr"]);
    let head = git(&work, &["rev-parse", "HEAD"]);
    git(&work, &["checkout", "-q", "base"]);
    git(&work, &["config", "user.email", "bob@example.com"]);
    git(&work, &["config", "user.name", "bob"]);
    write_hunk_fns(&work, n, 2);
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

type WsSink = futures_util::stream::SplitSink<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    Message,
>;
type WsStream = futures_util::stream::SplitStream<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
>;

async fn connect_cookie(
    addr: std::net::SocketAddr,
    match_id: &str,
    cookie: &str,
) -> (WsSink, WsStream) {
    let url = format!("ws://{addr}/ws?match={match_id}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Cookie", format!("git_fight_sid={cookie}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.split()
}

async fn wait_ws_type(stream: &mut WsStream, ty: &str) -> Value {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timeout waiting for ws")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some(ty) {
            return v;
        }
    }
}

fn spawn_hello_drain(mut stream: WsStream) -> tokio::sync::mpsc::Receiver<Value> {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if matches!(v["type"].as_str(), Some("hello" | "end")) && tx.send(v).await.is_err() {
                break;
            }
        }
    });
    rx
}

async fn send_buttons(alice: &mut WsSink, other: &mut WsSink, tick: u32, ours: u8, theirs: u8) {
    let o = format!(r#"{{"type":"input","tick":{tick},"buttons":{ours}}}"#);
    let t = format!(r#"{{"type":"input","tick":{tick},"buttons":{theirs}}}"#);
    alice.send(Message::Text(o.into())).await.unwrap();
    other.send(Message::Text(t.into())).await.unwrap();
}

struct MockOpts {
    commit_author: Value,
    size: u64,
    auto_challenge: bool,
    commit_authors: Vec<(String, Value)>,
    mergeable: Vec<Value>,
}

#[derive(Debug)]
struct PullMergeable {
    head: String,
    base: String,
    mergeable: Mutex<Vec<Value>>,
}

impl Respond for PullMergeable {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let mut seq = self.mergeable.lock().unwrap();
        let mergeable = if seq.len() > 1 {
            seq.remove(0)
        } else {
            seq.first().cloned().unwrap_or(json!(false))
        };
        ResponseTemplate::new(200).set_body_json(json!({
            "number": 1,
            "mergeable": mergeable,
            "head": { "sha": self.head, "ref": "pr" },
            "base": { "sha": self.base, "ref": "main" },
            "user": { "login": "alice" }
        }))
    }
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
    let mergeable = if opts.mergeable.is_empty() {
        vec![json!(false)]
    } else {
        opts.mergeable
    };
    Mock::given(method("GET"))
        .and(path("/repos/acme/box/pulls/1"))
        .respond_with(PullMergeable {
            head: head.to_string(),
            base: base.to_string(),
            mergeable: Mutex::new(mergeable),
        })
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
        mergeable: vec![],
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
            session_key: SESSION_KEY.to_vec(),
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
async fn fight_comment_two_files_plays_two_rounds_and_pushes_both() {
    let (_keep, bare, head, base, _, _) = conflict_two_authors();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let mut cfg = cfg_for(&mock, bare.clone());
    cfg.instant = true;
    let (addr, pool) = spawn_with_pool(cfg).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-two-files", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("2 rounds") && t.contains("/match/")),
        "{comments:?}"
    );
    let id = match_id_from(&comments);
    let hunks = git_fight_server::db::list_hunks(&pool, &id).await.unwrap();
    let paths: Vec<&str> = hunks.iter().map(|h| h.path.as_str()).collect();
    assert_eq!(hunks.len(), 2, "{paths:?}");
    assert!(
        paths.contains(&"a.rs") && paths.contains(&"b.rs"),
        "{paths:?}"
    );

    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    let cookie = git_fight_server::sign_session(SESSION_KEY, "sid-alice");
    let url = format!("ws://{addr}/ws?match={id}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Cookie", format!("git_fight_sid={cookie}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
    let mut next_send = 0u32;
    let mut ends = 0u32;
    let mut hellos = 0u32;
    loop {
        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timeout waiting for two-round match")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("hello") => {
                hellos += 1;
                assert_eq!(v["your_role"].as_str(), Some("ours"), "{v}");
                assert_eq!(v["total_rounds"].as_u64(), Some(2), "{v}");
                next_send = 0;
                while next_send < 16 {
                    let body = format!(r#"{{"type":"input","tick":{next_send},"buttons":2}}"#);
                    sink.send(Message::Text(body.into())).await.unwrap();
                    next_send += 1;
                }
            }
            Some("tick") => {
                let n = v["n"].as_u64().unwrap() as u32;
                while next_send <= n + 8 {
                    let body = format!(r#"{{"type":"input","tick":{next_send},"buttons":2}}"#);
                    sink.send(Message::Text(body.into())).await.unwrap();
                    next_send += 1;
                }
            }
            Some("end") => {
                ends += 1;
                let over = v["match_over"].as_bool() == Some(true);
                if over {
                    assert_eq!(ends, 2, "{v}");
                    break;
                }
                assert_eq!(v["round"].as_u64(), Some(0), "{v}");
            }
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }
    assert_eq!(hellos, 2, "expected Hello for each round");
    assert_eq!(ends, 2);

    let mut branch = None;
    for _ in 0..80 {
        let row = git_fight_server::db::get_match(&pool, &id)
            .await
            .unwrap()
            .unwrap();
        if row.result_branch.is_some() || row.abort_reason.is_some() {
            assert!(row.abort_reason.is_none(), "unexpected abort {row:?}");
            branch = row.result_branch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let branch = branch.expect("result branch after two rounds");
    assert!(branch.starts_with("git-fight/pr-1-"), "{branch}");
    let a = git_dir(&bare, &["show", &format!("{branch}:a.rs")]);
    let b = git_dir(&bare, &["show", &format!("{branch}:b.rs")]);
    assert!(!a.contains("<<<<<<<"), "a.rs still conflicted: {a}");
    assert!(!b.contains("<<<<<<<"), "b.rs still conflicted: {b}");
    assert!(a.contains("fn a()"), "{a}");
    assert!(b.contains("fn b()"), "{b}");
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/pr"]), head);
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/base"]), base);

    let patched = wait_patched(&mock, 1).await;
    assert!(
        patched.iter().any(|c| {
            c.contains("git fight finished")
                && c.contains("a.rs")
                && c.contains("b.rs")
                && c.contains(&branch)
        }),
        "{patched:?}"
    );
}

#[tokio::test]
async fn fight_comment_two_authors_play_two_files_and_push() {
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
            mergeable: vec![],
        },
    )
    .await;
    let mut cfg = cfg_for(&mock, bare.clone());
    cfg.instant = true;
    let (addr, pool) = spawn_with_pool(cfg).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-two-authors", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("2 rounds") && t.contains("/match/")),
        "{comments:?}"
    );
    let id = match_id_from(&comments);
    let hunks = git_fight_server::db::list_hunks(&pool, &id).await.unwrap();
    assert_eq!(
        hunks.len(),
        2,
        "{:?}",
        hunks.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    assert_eq!(hunks[0].path, "a.rs");
    assert_eq!(hunks[0].theirs_login.as_deref(), Some("bob"));
    assert_eq!(hunks[1].path, "b.rs");
    assert_eq!(hunks[1].theirs_login.as_deref(), Some("carol"));
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-bob", 2, "bob")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-carol", 3, "carol")
        .await
        .unwrap();
    let alice_c = git_fight_server::sign_session(SESSION_KEY, "sid-alice");
    let bob_c = git_fight_server::sign_session(SESSION_KEY, "sid-bob");
    let carol_c = git_fight_server::sign_session(SESSION_KEY, "sid-carol");

    let (mut alice_sink, mut alice_stream) = connect_cookie(addr, &id, &alice_c).await;
    let (mut bob_sink, mut bob_stream) = connect_cookie(addr, &id, &bob_c).await;
    let (mut carol_sink, mut carol_stream) = connect_cookie(addr, &id, &carol_c).await;

    let alice_h = wait_ws_type(&mut alice_stream, "hello").await;
    let bob_h = wait_ws_type(&mut bob_stream, "hello").await;
    let carol_h = wait_ws_type(&mut carol_stream, "hello").await;
    assert_eq!(alice_h["your_role"].as_str(), Some("ours"), "{alice_h}");
    assert_eq!(bob_h["your_role"].as_str(), Some("theirs"), "{bob_h}");
    assert_eq!(
        carol_h["your_role"].as_str(),
        Some("spectator"),
        "{carol_h}"
    );
    let mut bob_rx = spawn_hello_drain(bob_stream);
    let mut carol_rx = spawn_hello_drain(carol_stream);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
    let mut next_send = 0u32;
    while next_send < 16 {
        let ours = format!(r#"{{"type":"input","tick":{next_send},"buttons":2}}"#);
        let idle = format!(r#"{{"type":"input","tick":{next_send},"buttons":0}}"#);
        alice_sink.send(Message::Text(ours.into())).await.unwrap();
        bob_sink.send(Message::Text(idle.into())).await.unwrap();
        next_send += 1;
    }
    let mut ends = 0u32;
    let mut theirs_is_carol = false;
    loop {
        let msg = tokio::time::timeout_at(deadline, alice_stream.next())
            .await
            .expect("timeout waiting for two-author match")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("hello") => {
                next_send = 0;
                while next_send < 16 {
                    let ours = format!(r#"{{"type":"input","tick":{next_send},"buttons":2}}"#);
                    let idle = format!(r#"{{"type":"input","tick":{next_send},"buttons":0}}"#);
                    alice_sink.send(Message::Text(ours.into())).await.unwrap();
                    if theirs_is_carol {
                        carol_sink.send(Message::Text(idle.into())).await.unwrap();
                    } else {
                        bob_sink.send(Message::Text(idle.into())).await.unwrap();
                    }
                    next_send += 1;
                }
            }
            Some("tick") => {
                let n = v["n"].as_u64().unwrap() as u32;
                while next_send <= n + 8 {
                    let ours = format!(r#"{{"type":"input","tick":{next_send},"buttons":2}}"#);
                    let idle = format!(r#"{{"type":"input","tick":{next_send},"buttons":0}}"#);
                    alice_sink.send(Message::Text(ours.into())).await.unwrap();
                    if theirs_is_carol {
                        carol_sink.send(Message::Text(idle.into())).await.unwrap();
                    } else {
                        bob_sink.send(Message::Text(idle.into())).await.unwrap();
                    }
                    next_send += 1;
                }
            }
            Some("end") => {
                ends += 1;
                if v["match_over"].as_bool() == Some(true) {
                    assert_eq!(ends, 2, "{v}");
                    break;
                }
                assert_eq!(v["round"].as_u64(), Some(0), "{v}");
                theirs_is_carol = true;
            }
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }
    assert_eq!(ends, 2);
    let bob_later = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let v = bob_rx.recv().await.expect("bob closed");
            if v["type"].as_str() == Some("hello") {
                return v;
            }
        }
    })
    .await
    .expect("bob round-2 hello");
    let carol_later = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let v = carol_rx.recv().await.expect("carol closed");
            if v["type"].as_str() == Some("hello") {
                return v;
            }
        }
    })
    .await
    .expect("carol round-2 hello");
    assert_eq!(
        bob_later["your_role"].as_str(),
        Some("spectator"),
        "{bob_later}"
    );
    assert_eq!(
        carol_later["your_role"].as_str(),
        Some("theirs"),
        "{carol_later}"
    );

    let mut branch = None;
    for _ in 0..80 {
        let row = git_fight_server::db::get_match(&pool, &id)
            .await
            .unwrap()
            .unwrap();
        if row.result_branch.is_some() || row.abort_reason.is_some() {
            assert!(row.abort_reason.is_none(), "unexpected abort {row:?}");
            branch = row.result_branch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let branch = branch.expect("result branch after two author rounds");
    assert!(branch.starts_with("git-fight/pr-1-"), "{branch}");
    assert_eq!(
        git_dir(&bare, &["show", &format!("{branch}:a.rs")]),
        "fn a() { 2 }",
        "alice (PR) should take a.rs"
    );
    assert_eq!(
        git_dir(&bare, &["show", &format!("{branch}:b.rs")]),
        "fn b() { 2 }",
        "alice (PR) should take b.rs"
    );
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/pr"]), head);
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/base"]), base);
    let parents = git_dir(&bare, &["rev-list", "--parents", "-n1", &branch]);
    let parts: Vec<&str> = parents.split_whitespace().collect();
    assert_eq!(parts.len(), 3, "{parents}");
    assert!(parts.contains(&head.as_str()), "{parents}");
    assert!(parts.contains(&base.as_str()), "{parents}");
    let msg = git_dir(&bare, &["log", "-1", "--format=%B", &branch]);
    assert!(msg.contains("round 1: a.rs"), "{msg}");
    assert!(msg.contains("round 2: b.rs"), "{msg}");

    let patched = wait_patched(&mock, 1).await;
    assert!(
        patched.iter().any(|c| {
            c.contains("git fight finished")
                && c.contains("compare:")
                && c.contains(&format!("/replay/{id}"))
                && c.contains(&branch)
                && c.contains("a.rs")
                && c.contains("b.rs")
        }),
        "{patched:?}"
    );
    let (status, body) = http(addr, "GET", &format!("/api/replays/{id}"), &[], b"").await;
    assert_eq!(status, 200);
    let replay: Value = serde_json::from_slice(&body).unwrap();
    let rounds = replay["rounds"].as_array().cloned().unwrap_or_default();
    assert_eq!(rounds.len(), 2, "{replay}");
    let paths: Vec<&str> = rounds.iter().filter_map(|r| r["path"].as_str()).collect();
    assert!(
        paths.contains(&"a.rs") && paths.contains(&"b.rs"),
        "{paths:?}"
    );

    let board = git_fight_server::db::list_player_stats(&pool, "acme", "box")
        .await
        .unwrap();
    let alice = board.iter().find(|p| p.github_login == "alice");
    let bob = board.iter().find(|p| p.github_login == "bob");
    let carol = board.iter().find(|p| p.github_login == "carol");
    assert_eq!(alice.map(|p| (p.wins, p.losses)), Some((2, 0)), "{board:?}");
    assert_eq!(
        bob.map(|p| (p.wins, p.losses, p.conflicts_caused)),
        Some((0, 1, 1)),
        "{board:?}"
    );
    assert_eq!(
        carol.map(|p| (p.wins, p.losses, p.conflicts_caused)),
        Some((0, 1, 1)),
        "{board:?}"
    );
    let (status, body) = http(
        addr,
        "GET",
        "/acme/box/leaderboard",
        &[("Accept", "application/json")],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["players"][0]["login"], "alice");
    assert_eq!(v["players"][0]["wins"], 2);
}

#[tokio::test]
async fn fight_comment_two_authors_mixed_picks_push() {
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
            mergeable: vec![],
        },
    )
    .await;
    let mut cfg = cfg_for(&mock, bare.clone());
    cfg.instant = true;
    let (addr, pool) = spawn_with_pool(cfg).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-mixed-authors", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    let id = match_id_from(&comments);
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-bob", 2, "bob")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-carol", 3, "carol")
        .await
        .unwrap();
    let alice_c = git_fight_server::sign_session(SESSION_KEY, "sid-alice");
    let bob_c = git_fight_server::sign_session(SESSION_KEY, "sid-bob");
    let carol_c = git_fight_server::sign_session(SESSION_KEY, "sid-carol");

    let (mut alice_sink, mut alice_stream) = connect_cookie(addr, &id, &alice_c).await;
    let (mut bob_sink, mut bob_stream) = connect_cookie(addr, &id, &bob_c).await;
    let (mut carol_sink, mut carol_stream) = connect_cookie(addr, &id, &carol_c).await;
    let _ = wait_ws_type(&mut alice_stream, "hello").await;
    let _ = wait_ws_type(&mut bob_stream, "hello").await;
    let _ = wait_ws_type(&mut carol_stream, "hello").await;
    let _bob_rx = spawn_hello_drain(bob_stream);
    let _carol_rx = spawn_hello_drain(carol_stream);

    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(40);
    let mut next_send = 0u32;
    let mut ours_btn = 2u8;
    let mut theirs_btn = 0u8;
    let mut theirs_is_carol = false;
    while next_send < 16 {
        send_buttons(
            &mut alice_sink,
            &mut bob_sink,
            next_send,
            ours_btn,
            theirs_btn,
        )
        .await;
        next_send += 1;
    }
    let mut ends = 0u32;
    loop {
        let msg = tokio::time::timeout_at(deadline, alice_stream.next())
            .await
            .expect("timeout waiting for mixed-pick match")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("hello") => {
                next_send = 0;
                while next_send < 16 {
                    let other = if theirs_is_carol {
                        &mut carol_sink
                    } else {
                        &mut bob_sink
                    };
                    send_buttons(&mut alice_sink, other, next_send, ours_btn, theirs_btn).await;
                    next_send += 1;
                }
            }
            Some("tick") => {
                let n = v["n"].as_u64().unwrap() as u32;
                while next_send <= n + 8 {
                    let other = if theirs_is_carol {
                        &mut carol_sink
                    } else {
                        &mut bob_sink
                    };
                    send_buttons(&mut alice_sink, other, next_send, ours_btn, theirs_btn).await;
                    next_send += 1;
                }
            }
            Some("end") => {
                ends += 1;
                if v["match_over"].as_bool() == Some(true) {
                    assert_eq!(ends, 2, "{v}");
                    break;
                }
                assert_eq!(v["round"].as_u64(), Some(0), "{v}");
                theirs_is_carol = true;
                ours_btn = 0;
                theirs_btn = 2;
            }
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }
    assert_eq!(ends, 2);

    let mut branch = None;
    for _ in 0..80 {
        let row = git_fight_server::db::get_match(&pool, &id)
            .await
            .unwrap()
            .unwrap();
        if row.result_branch.is_some() || row.abort_reason.is_some() {
            assert!(row.abort_reason.is_none(), "unexpected abort {row:?}");
            branch = row.result_branch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let hunks = git_fight_server::db::list_hunks(&pool, &id).await.unwrap();
    let winners: Vec<Option<String>> = hunks.iter().map(|h| h.winner.clone()).collect();
    assert_eq!(winners[0].as_deref(), Some("ours"), "{winners:?}");
    assert_eq!(winners[1].as_deref(), Some("theirs"), "{winners:?}");
    let branch = branch.expect("result branch after mixed picks");
    assert_eq!(
        git_dir(&bare, &["show", &format!("{branch}:a.rs")]),
        "fn a() { 2 }",
        "alice win on a.rs is the PR side"
    );
    assert_eq!(
        git_dir(&bare, &["show", &format!("{branch}:b.rs")]),
        "fn b() { 3 }",
        "carol win on b.rs is the base side"
    );
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/pr"]), head);
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/base"]), base);
}

#[tokio::test]
async fn duplicate_delivery_is_ignored() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let body = fight_body();
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-dup", &body).await,
        200
    );
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-dup", &body).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let later = posted_comments(&mock.received_requests().await.unwrap_or_default());
    assert_eq!(
        later.len(),
        1,
        "replayed delivery started a second fight: {later:?}"
    );
    assert!(
        comments.iter().any(|t| t.contains("/match/")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn mergeable_pr_comments_nothing_to_fight() {
    let mock = github_mocks(
        "dead",
        "beef",
        MockOpts {
            commit_author: Value::Null,
            size: 12,
            auto_challenge: false,
            commit_authors: vec![],
            mergeable: vec![json!(true)],
        },
    )
    .await;
    let addr = spawn(cfg_for(&mock, PathBuf::from("/nope"))).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-mergeable", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("no conflicts to fight")),
        "{comments:?}"
    );
    assert!(
        comments.iter().all(|t| !t.contains("/match/")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn mergeable_null_then_false_starts_fight() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(
        &head,
        &base,
        MockOpts {
            commit_author: Value::Null,
            size: 12,
            auto_challenge: false,
            commit_authors: vec![],
            mergeable: vec![Value::Null, json!(false)],
        },
    )
    .await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-poll-mergeable", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("/match/")),
        "{comments:?}"
    );
    let pulls = mock
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "GET" && r.url.path() == "/repos/acme/box/pulls/1")
        .count();
    assert!(
        pulls >= 2,
        "expected mergeable poll, got {pulls} GET /pulls/1"
    );
}

#[tokio::test]
async fn binary_conflict_is_not_fightable() {
    let (_keep, bare, head, base) = binary_conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-binary", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("not the kind git fight can play")),
        "{comments:?}"
    );
    assert!(
        git_fight_server::db::open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_none(),
        "unfightable conflicts must not leave a pending match"
    );
}

#[tokio::test]
async fn too_many_conflicts_comment_and_abort() {
    let (_keep, bare, head, base) = many_hunks_bare(16);
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-too-many", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("too many conflicts for one fight") && t.contains("max 15")),
        "{comments:?}"
    );
    assert!(
        comments.iter().all(|t| !t.contains("/match/")),
        "{comments:?}"
    );
    assert!(
        git_fight_server::db::open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_none(),
        "too many conflicts must not leave a pending match"
    );
}

#[tokio::test]
async fn fifteen_hunks_starts_a_match() {
    let (_keep, bare, head, base) = many_hunks_bare(15);
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-fifteen", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("15 rounds") && t.contains("/match/")),
        "{comments:?}"
    );
    let id = match_id_from(&comments);
    let hunks = git_fight_server::db::list_hunks(&pool, &id).await.unwrap();
    assert_eq!(
        hunks.len(),
        15,
        "{:?}",
        hunks.iter().map(|h| &h.path).collect::<Vec<_>>()
    );
    assert!(
        git_fight_server::db::open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn modify_delete_conflict_is_not_fightable() {
    let (_keep, bare, head, base) = modify_delete_conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, bare)).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-mod-del", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("not the kind git fight can play")
                || t.contains("no conflicts to fight")),
        "{comments:?}"
    );
    assert!(
        git_fight_server::db::open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_none(),
        "modify-delete must not leave a pending match"
    );
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
            mergeable: vec![],
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
            mergeable: vec![],
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
            mergeable: vec![],
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
            mergeable: vec![],
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

#[tokio::test]
async fn second_fight_while_cloning_gets_open_link() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let addr = spawn(cfg_for(&mock, bare)).await;
    let body_a = fight_body();
    let body_b = fight_body();
    let a = post_signed(addr, "issue_comment", "deliv-race-a", &body_a);
    let b = post_signed(addr, "issue_comment", "deliv-race-b", &body_b);
    let (sa, sb) = tokio::join!(a, b);
    assert_eq!(sa, 200);
    assert_eq!(sb, 200);
    let comments = wait_posted(&mock, 2).await;
    assert!(
        comments
            .iter()
            .any(|t| t.contains("git fight:") && t.contains("/match/")),
        "{comments:?}"
    );
    assert!(
        comments.iter().any(|t| t.contains("already open")),
        "{comments:?}"
    );
}

#[tokio::test]
async fn failed_clone_aborts_open_match() {
    let mock = github_mocks(
        "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        cpu_opts(),
    )
    .await;
    let (addr, pool) = spawn_with_pool(cfg_for(&mock, PathBuf::from("/nope"))).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-clone-fail", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    assert!(
        comments.iter().any(|t| t.contains("could not start")),
        "{comments:?}"
    );
    assert!(
        git_fight_server::db::open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_none(),
        "failed clone must not leave a pending match"
    );
}

#[tokio::test]
async fn fight_comment_plays_and_pushes_create_only_branch() {
    let (_keep, bare, head, base) = conflict_bare();
    let mock = github_mocks(&head, &base, cpu_opts()).await;
    let mut cfg = cfg_for(&mock, bare.clone());
    cfg.instant = true;
    let (addr, pool) = spawn_with_pool(cfg).await;
    assert_eq!(
        post_signed(addr, "issue_comment", "deliv-e2e", &fight_body()).await,
        200
    );
    let comments = wait_posted(&mock, 1).await;
    let id = match_id_from(&comments);
    assert_eq!(wait_challenge_comment_id(&pool, &id).await, Some(99));

    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    let cookie = git_fight_server::sign_session(SESSION_KEY, "sid-alice");
    let url = format!("ws://{addr}/ws?match={id}");
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
            .expect("timeout waiting for match")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("hello") => {
                assert_eq!(v["your_role"].as_str(), Some("ours"), "{v}");
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
                assert_eq!(v["match_over"].as_bool(), Some(true), "{v}");
                break;
            }
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }

    let mut branch = None;
    for _ in 0..50 {
        let row = git_fight_server::db::get_match(&pool, &id)
            .await
            .unwrap()
            .unwrap();
        if row.result_branch.is_some() || row.abort_reason.is_some() {
            assert!(row.abort_reason.is_none(), "unexpected abort {row:?}");
            branch = row.result_branch;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let branch = branch.expect("result branch after last round");
    assert!(branch.starts_with("git-fight/pr-1-"), "{branch}");
    assert!(
        git_dir(&bare, &["show-ref", "--heads"])
            .lines()
            .any(|l| l.ends_with(&format!("refs/heads/{branch}"))),
        "missing {branch} in {}",
        git_dir(&bare, &["show-ref", "--heads"])
    );
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/pr"]), head);
    assert_eq!(git_dir(&bare, &["rev-parse", "refs/heads/base"]), base);

    let patched = wait_patched(&mock, 1).await;
    assert!(
        patched.iter().any(|c| {
            c.contains("git fight finished")
                && c.contains("compare:")
                && c.contains(&branch)
                && c.contains(&format!("/replay/{id}"))
        }),
        "{patched:?}"
    );
}
