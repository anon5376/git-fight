use futures_util::{SinkExt, StreamExt};
use git_fight_core::FighterStats;
use git_fight_server::db::{NewHunk, NewMatch};
use git_fight_server::{sign_session, Auth, Config};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &[u8] = b"session-key-session-key-session!";
const APP_PEM: &str = include_str!("fixtures/app_key.txt");

async fn spawn() -> (std::net::SocketAddr, sqlx::SqlitePool) {
    spawn_cfg(Config {
        auth: Auth {
            session_key: KEY.to_vec(),
            public_url: "http://fight.test".into(),
        },
        ..Config::default()
    })
    .await
}

async fn spawn_cfg(cfg: Config) -> (std::net::SocketAddr, sqlx::SqlitePool) {
    let dir = std::env::temp_dir().join(format!(
        "gf-roles-{}-{}",
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
    let cfg = Config {
        auth: Auth {
            session_key: KEY.to_vec(),
            public_url: "http://fight.test".into(),
        },
        ..cfg
    };
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

async fn github_match(pool: &sqlx::SqlitePool) {
    git_fight_server::db::insert_full_match(
        pool,
        &NewMatch {
            id: "match1".into(),
            seed: 1,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: Some("alice".into()),
            theirs_login: Some("bob".into()),
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
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
}

async fn session(pool: &sqlx::SqlitePool, login: &str) -> String {
    let sid = format!("sid-{login}");
    git_fight_server::db::insert_session(pool, &sid, 1, login)
        .await
        .unwrap();
    sign_session(KEY, &sid)
}

async fn hello_role(
    addr: std::net::SocketAddr,
    cookie: Option<&str>,
    token: Option<&str>,
) -> String {
    let mut url = format!("ws://{addr}/ws?match=match1");
    if let Some(t) = token {
        url.push_str("&token=");
        url.push_str(t);
    }
    let mut req = url.into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut()
            .insert("Cookie", format!("git_fight_sid={c}").parse().unwrap());
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    let (_, mut stream) = ws.split();
    loop {
        let msg = stream.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some("hello") {
            return v["your_role"].as_str().unwrap().to_string();
        }
    }
}

#[tokio::test]
async fn github_match_roles_follow_session_not_token() {
    let (addr, pool) = spawn().await;
    github_match(&pool).await;
    let alice = session(&pool, "alice").await;
    let bob = session(&pool, "bob").await;
    let carol = session(&pool, "carol").await;

    assert_eq!(hello_role(addr, None, None).await, "spectator");
    assert_eq!(
        hello_role(addr, None, Some("ours-token")).await,
        "spectator"
    );
    assert_eq!(hello_role(addr, Some(&alice), None).await, "ours");
    assert_eq!(hello_role(addr, Some(&bob), None).await, "theirs");
    assert_eq!(hello_role(addr, Some(&carol), None).await, "spectator");
    assert_eq!(
        hello_role(addr, Some(&alice), Some("theirs-token")).await,
        "ours"
    );
}

#[tokio::test]
async fn later_round_gives_theirs_slot_to_that_hunk_author() {
    let (addr, pool) = spawn_cfg(Config {
        instant: true,
        ..Config::default()
    })
    .await;
    github_match(&pool).await;
    for round in 0..2 {
        let login = if round == 0 { "bob" } else { "carol" };
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: "match1",
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: Some(login),
                theirs_name: Some(login),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    let alice = session(&pool, "alice").await;
    let bob = session(&pool, "bob").await;
    let carol = session(&pool, "carol").await;

    let (mut alice_sink, mut alice_stream) = connect(addr, Some(&alice)).await;
    let (mut bob_sink, mut bob_stream) = connect(addr, Some(&bob)).await;
    let (_carol_sink, mut carol_stream) = connect(addr, Some(&carol)).await;

    assert_eq!(
        wait_type(&mut alice_stream, "hello").await["your_role"].as_str(),
        Some("ours")
    );
    assert_eq!(
        wait_type(&mut bob_stream, "hello").await["your_role"].as_str(),
        Some("theirs")
    );
    assert_eq!(
        wait_type(&mut carol_stream, "hello").await["your_role"].as_str(),
        Some("spectator")
    );

    let mut next_send = 0u32;
    let mut confirmed: i32 = -1;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(20);
    let end = loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let msg = format!(r#"{{"type":"input","tick":{next_send},"buttons":0}}"#);
            alice_sink
                .send(Message::Text(msg.clone().into()))
                .await
                .unwrap();
            bob_sink.send(Message::Text(msg.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }
        let msg = tokio::time::timeout_at(deadline, alice_stream.next())
            .await
            .expect("timeout waiting for round 0")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("tick") => confirmed = v["n"].as_i64().unwrap_or(0) as i32,
            Some("end") => break v,
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    };
    assert_eq!(end["match_over"].as_bool(), Some(false));
    assert_eq!(
        wait_type(&mut alice_stream, "hello").await["your_role"].as_str(),
        Some("ours")
    );
    assert_eq!(
        wait_type(&mut bob_stream, "hello").await["your_role"].as_str(),
        Some("spectator")
    );
    assert_eq!(
        wait_type(&mut carol_stream, "hello").await["your_role"].as_str(),
        Some("theirs")
    );
}

async fn connect(
    addr: std::net::SocketAddr,
    cookie: Option<&str>,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    >,
) {
    let url = format!("ws://{addr}/ws?match=match1");
    let mut req = url.into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut()
            .insert("Cookie", format!("git_fight_sid={c}").parse().unwrap());
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.split()
}

async fn wait_type(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    ty: &str,
) -> Value {
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

#[tokio::test]
async fn oauth_me_roundtrip() {
    let (addr, pool) = spawn().await;
    let cookie = session(&pool, "alice").await;
    let (status, _, body) = http_ex(
        addr,
        "GET",
        "/api/me",
        &[("Cookie", &format!("git_fight_sid={cookie}"))],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("alice"), "{body}");
}

async fn http_ex(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> (u16, Vec<(String, String)>, Vec<u8>) {
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
    let (head, rest) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let hdrs = head
        .lines()
        .skip(1)
        .filter_map(|l| l.split_once(": "))
        .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
        .collect();
    (status, hdrs, rest.as_bytes().to_vec())
}

fn cookie_value(headers: &[(String, String)], name: &str) -> Option<String> {
    let prefix = format!("{name}=");
    for (k, v) in headers {
        if k == "set-cookie" {
            if let Some(rest) = v.strip_prefix(&prefix) {
                return Some(rest.split(';').next().unwrap_or(rest).to_string());
            }
        }
    }
    None
}

#[tokio::test]
async fn github_oauth_callback_sets_session() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "ghu_test"
        })))
        .mount(&mock)
        .await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": 7,
            "login": "alice"
        })))
        .mount(&mock)
        .await;
    let (addr, _pool) = spawn_cfg(Config {
        github: Some(git_fight_server::GitHub::new(
            mock.uri(),
            mock.uri(),
            1,
            APP_PEM.to_string(),
            "cid".into(),
            "csec".into(),
        )),
        ..Config::default()
    })
    .await;
    let (status, headers, _) =
        http_ex(addr, "GET", "/auth/github?return=/match/m1", &[], b"").await;
    assert_eq!(status, 302);
    let loc = headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(loc.contains("/login/oauth/authorize"), "{loc}");
    let nonce = loc
        .split("state=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap()
        .to_string();
    let oauth_cookie = cookie_value(&headers, "git_fight_oauth").expect("oauth cookie");
    let (status, headers, _) = http_ex(
        addr,
        "GET",
        &format!("/auth/github/callback?code=abc&state={nonce}"),
        &[("Cookie", &format!("git_fight_oauth={oauth_cookie}"))],
        b"",
    )
    .await;
    assert_eq!(status, 302);
    let dest = headers
        .iter()
        .find(|(k, _)| k == "location")
        .map(|(_, v)| v.as_str())
        .unwrap();
    assert_eq!(dest, "/match/m1");
    let sid = cookie_value(&headers, "git_fight_sid").expect("session cookie");
    let (status, _, body) = http_ex(
        addr,
        "GET",
        "/api/me",
        &[("Cookie", &format!("git_fight_sid={sid}"))],
        b"",
    )
    .await;
    assert_eq!(status, 200);
    assert!(String::from_utf8_lossy(&body).contains("alice"));
}
