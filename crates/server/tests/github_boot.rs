//! The live GitHub App process is env-wired in `main`. Library `serve()`
//! tests cannot see a missing SESSION_KEY or a PEM that arrived as `\n`.

use std::net::SocketAddr;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::Command;

const PEM: &str = include_str!("fixtures/app_key.txt");

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_git-fight-server")
}

async fn read_listen_addr(stderr: impl tokio::io::AsyncRead + Unpin) -> Option<SocketAddr> {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if let Some(rest) = line.strip_prefix("git-fight-server on http://") {
            return rest.trim().parse().ok();
        }
    }
    None
}

async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<u8>, Option<String>) {
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
    let location = text
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("location:"))
        .map(|l| l[9..].trim().to_string());
    let rest = text.split("\r\n\r\n").nth(1).unwrap_or("");
    (status, rest.as_bytes().to_vec(), location)
}

fn github_env(cmd: &mut Command, db: &str, pem: &str) {
    cmd.args(["--bind", "127.0.0.1:0", "--db", db])
        .env("GITHUB_APP_ID", "1")
        .env("GITHUB_APP_PRIVATE_KEY", pem)
        .env("GITHUB_CLIENT_ID", "Iv1.testclient")
        .env("GITHUB_CLIENT_SECRET", "client-secret-for-tests")
        .env("GITHUB_WEBHOOK_SECRET", "webhook-secret-for-tests")
        .env("SESSION_KEY", "session-key-session-key-session!")
        .env("GIT_FIGHT_PUBLIC_URL", "http://127.0.0.1:8080")
        .env_remove("GITHUB_API_URL")
        .env_remove("GITHUB_OAUTH_URL");
}

#[tokio::test]
async fn github_app_env_boots_fail_closed() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot");
    let db = format!("sqlite://{}/m.db", dir.display());
    // Hosting often injects the PEM as a single line with `\n`.
    let pem = PEM.replace('\n', "\\n");
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    github_env(&mut cmd, &db, &pem);
    let mut child = cmd.spawn().unwrap();
    let stderr = child.stderr.take().expect("stderr");
    let addr = tokio::time::timeout(Duration::from_secs(15), read_listen_addr(stderr))
        .await
        .expect("server listen timeout")
        .expect("listen addr");
    let mut ready = false;
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready, "GitHub App process never accepted {addr}");
    let (health, _, _) = http(addr, "GET", "/health", &[], b"").await;
    assert_eq!(health, 200, "GitHub App mode must still serve /health");

    let (unsigned, _, _) = http(
        addr,
        "POST",
        "/webhooks/github",
        &[("Content-Type", "application/json")],
        b"{\"action\":\"created\"}",
    )
    .await;
    assert_eq!(unsigned, 401, "HMAC must reject before JSON");

    let (create, body, _) = http(
        addr,
        "POST",
        "/api/matches",
        &[("Content-Type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(create, 404, "GitHub-backed host must not mint local tokens");
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("ours_token"), "{text}");
    assert!(!text.contains("theirs_token"), "{text}");

    let (auth, _, loc) = http(addr, "GET", "/auth/github?return=/match/abc", &[], b"").await;
    assert_eq!(auth, 302, "OAuth start must redirect");
    let loc = loc.expect("Location");
    assert!(
        loc.starts_with("https://github.com/login/oauth/authorize?"),
        "live OAuth must pin github.com: {loc}"
    );
    assert!(loc.contains("code_challenge_method=S256"), "{loc}");
    assert!(loc.contains("redirect_uri="), "{loc}");
}

#[tokio::test]
async fn github_app_env_without_session_key_exits() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot-nosession");
    let db = format!("sqlite://{}/m.db", dir.display());
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    github_env(&mut cmd, &db, PEM);
    cmd.env_remove("SESSION_KEY");
    let status = tokio::time::timeout(Duration::from_secs(15), cmd.status())
        .await
        .expect("exit timeout")
        .expect("status");
    assert!(
        !status.success(),
        "a GitHub App process without SESSION_KEY must not listen"
    );
}

#[tokio::test]
async fn github_app_env_junk_app_id_exits() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot-junkid");
    let db = format!("sqlite://{}/m.db", dir.display());
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    github_env(&mut cmd, &db, PEM);
    cmd.env("GITHUB_APP_ID", "not-a-number");
    let status = tokio::time::timeout(Duration::from_secs(15), cmd.status())
        .await
        .expect("exit timeout")
        .expect("status");
    assert!(
        !status.success(),
        "a non-numeric GITHUB_APP_ID must not start as a local demo"
    );
}

#[tokio::test]
async fn github_app_env_only_client_id_exits() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot-partial");
    let db = format!("sqlite://{}/m.db", dir.display());
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.args(["--bind", "127.0.0.1:0", "--db", &db])
        .env("GITHUB_CLIENT_ID", "Iv1.testclient")
        .env_remove("GITHUB_APP_ID")
        .env_remove("GITHUB_APP_PRIVATE_KEY")
        .env_remove("GITHUB_CLIENT_SECRET")
        .env_remove("GITHUB_WEBHOOK_SECRET")
        .env_remove("SESSION_KEY")
        .env_remove("GIT_FIGHT_PUBLIC_URL");
    let status = tokio::time::timeout(Duration::from_secs(15), cmd.status())
        .await
        .expect("exit timeout")
        .expect("status");
    assert!(
        !status.success(),
        "a partial GitHub App env must not start as a local demo"
    );
}

#[tokio::test]
async fn github_app_env_evil_api_url_exits() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot-evilapi");
    let db = format!("sqlite://{}/m.db", dir.display());
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    github_env(&mut cmd, &db, PEM);
    cmd.env("GITHUB_API_URL", "https://evil.example");
    let status = tokio::time::timeout(Duration::from_secs(15), cmd.status())
        .await
        .expect("exit timeout")
        .expect("status");
    assert!(
        !status.success(),
        "a GitHub App process must not talk to a non-github.com API"
    );
}

#[tokio::test]
async fn missing_app_env_is_local_demo() {
    let dir = git_fight_server::test_tmp_dir("gf-gh-boot-local");
    let db = format!("sqlite://{}/m.db", dir.display());
    let mut cmd = Command::new(bin());
    cmd.kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd.args(["--bind", "127.0.0.1:0", "--db", &db])
        .env_remove("GITHUB_APP_ID")
        .env_remove("GITHUB_APP_PRIVATE_KEY")
        .env_remove("GITHUB_CLIENT_ID")
        .env_remove("GITHUB_CLIENT_SECRET")
        .env_remove("GITHUB_WEBHOOK_SECRET")
        .env_remove("SESSION_KEY")
        .env_remove("GIT_FIGHT_PUBLIC_URL")
        .env_remove("GITHUB_API_URL")
        .env_remove("GITHUB_OAUTH_URL");
    let mut child = cmd.spawn().unwrap();
    let stderr = child.stderr.take().expect("stderr");
    let addr = tokio::time::timeout(Duration::from_secs(15), read_listen_addr(stderr))
        .await
        .expect("server listen timeout")
        .expect("listen addr");
    let mut ready = false;
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready, "local demo process never accepted {addr}");
    let (create, body, _) = http(
        addr,
        "POST",
        "/api/matches",
        &[("Content-Type", "application/json")],
        b"{}",
    )
    .await;
    assert_eq!(create, 200, "absent App env must still mint local tokens");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("ours_token"), "{text}");
    assert!(text.contains("theirs_token"), "{text}");
}
