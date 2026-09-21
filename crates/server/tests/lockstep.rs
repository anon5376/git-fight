use futures_util::{SinkExt, StreamExt};
use git_fight_core::{FightState, FighterStats, Input};
use git_fight_server::Config;
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn two_clients_agree_with_server_hash() {
    let addr = spawn_server(Config {
        instant: true,
        ..Config::default()
    })
    .await;

    let created: Value = http_post(addr, "/api/matches", r#"{"seed":1}"#).await.1;
    let id = created["id"].as_str().unwrap().to_string();
    let ours_token = created["ours_token"].as_str().unwrap().to_string();
    let theirs_token = created["theirs_token"].as_str().unwrap().to_string();

    let (ours, theirs) = tokio::join!(
        play(addr, &id, &ours_token, true),
        play(addr, &id, &theirs_token, false),
    );

    assert_eq!(ours, theirs, "clients diverged");
    let replay: Value = http_get(addr, &format!("/api/replays/{id}")).await.1;
    let hash = replay["final_hash"].as_str().expect("finished replay");
    let (hi, lo) = (ours >> 32, ours as u32);
    let expected = format!("{hi:08x}{lo:08x}");
    assert_eq!(hash, expected, "server hash {hash} != client {expected}");
}

#[tokio::test]
async fn unfinished_match_has_no_replay() {
    let addr = spawn_server(Config::default()).await;
    let created: Value = http_post(addr, "/api/matches", r#"{"seed":2}"#).await.1;
    let id = created["id"].as_str().unwrap();
    let (status, _) = http_get(addr, &format!("/api/replays/{id}")).await;
    assert_eq!(status, 404);
}

#[tokio::test]
async fn pending_match_expires_without_result() {
    let dir = std::env::temp_dir().join(format!("git-fight-exp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    git_fight_server::db::insert_match(&pool, "deadbeef", 1, 3, "o", "t", 0)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let ids = git_fight_server::db::expire_pending(&pool).await.unwrap();
    assert!(ids.iter().any(|id| id == "deadbeef"));
    let row = git_fight_server::db::get_match(&pool, "deadbeef")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "expired");
    assert!(row.final_hash.is_none());
}

#[tokio::test]
async fn disconnect_after_grace_period_forfeits_round() {
    let addr = spawn_server(Config {
        instant: true,
        disconnect: Duration::from_millis(80),
        ..Config::default()
    })
    .await;
    let created: Value = http_post(addr, "/api/matches", r#"{"seed":3}"#).await.1;
    let id = created["id"].as_str().unwrap().to_string();
    let ours_token = created["ours_token"].as_str().unwrap().to_string();
    let theirs_token = created["theirs_token"].as_str().unwrap().to_string();

    let ours_url = format!("ws://{addr}/ws?match={id}&token={ours_token}");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token={theirs_token}");
    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (theirs_ws, _) = tokio_tungstenite::connect_async(&theirs_url).await.unwrap();
    let (mut ours_sink, mut ours_stream) = ours_ws.split();
    let (mut theirs_sink, mut theirs_stream) = theirs_ws.split();
    let _ = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;

    for tick in 0..8u32 {
        let msg = format!(r#"{{"type":"input","tick":{tick},"buttons":0}}"#);
        ours_sink
            .send(Message::Text(msg.clone().into()))
            .await
            .unwrap();
        theirs_sink.send(Message::Text(msg.into())).await.unwrap();
    }

    drop(ours_sink);
    drop(ours_stream);

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout_at(deadline, theirs_stream.next())
            .await
            .expect("timeout waiting for forfeit")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some("end") {
            assert_eq!(
                v["result"].as_i64().unwrap(),
                1,
                "ours disconnect => theirs wins"
            );
            return;
        }
    }
}

async fn spawn_server(cfg: Config) -> std::net::SocketAddr {
    let dir =
        std::env::temp_dir().join(format!("git-fight-{}-{}", std::process::id(), uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(listener, pool, cfg).await.unwrap();
    });
    for _ in 0..100 {
        match TcpStream::connect(addr).await {
            Ok(mut s) => {
                let _ = s
                    .write_all(
                        b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
        }
    }
    addr
}

fn uuid_like() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

async fn play(addr: std::net::SocketAddr, id: &str, token: &str, is_ours: bool) -> u64 {
    let url = format!("ws://{addr}/ws?match={id}&token={token}");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (mut sink, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(
        hello["your_role"].as_str().unwrap(),
        if is_ours { "ours" } else { "theirs" }
    );
    let seed_lo = hello["seed_lo"].as_u64().unwrap() as u32;
    let seed_hi = hello["seed_hi"].as_u64().unwrap() as u32;
    let delay = hello["input_delay"].as_u64().unwrap() as u32;
    let seed = (u64::from(seed_hi) << 32) | u64::from(seed_lo);
    let mut sim = FightState::new(seed, FighterStats::default(), FighterStats::default());
    let mut next_send = 0u32;
    let mut confirmed: i32 = hello["confirmed_tick"].as_i64().unwrap() as i32;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + delay + 12;
        while next_send <= horizon {
            let buttons = if is_ours && next_send.is_multiple_of(14) {
                1
            } else {
                0
            };
            let msg = format!(r#"{{"type":"input","tick":{next_send},"buttons":{buttons}}}"#);
            sink.send(Message::Text(msg.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }

        let msg = tokio::time::timeout_at(deadline, stream.next())
            .await
            .expect("timeout waiting for server")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("tick") => {
                let ours = v["ours"].as_u64().unwrap() as u8;
                let theirs = v["theirs"].as_u64().unwrap() as u8;
                let n = v["n"].as_u64().unwrap() as u32;
                if n == sim.tick {
                    sim.step(Input::from_u8(ours), Input::from_u8(theirs));
                    confirmed = n as i32;
                }
            }
            Some("end") => {
                let hi = v["hash_hi"].as_u64().unwrap();
                let lo = v["hash_lo"].as_u64().unwrap();
                let server = (hi << 32) | lo;
                assert_eq!(sim.state_hash(), server, "local sim != server end hash");
                return server;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    }
}

async fn wait_type(
    stream: &mut (impl StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin),
    ty: &str,
) -> Value {
    loop {
        let msg = stream.next().await.unwrap().unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some(ty) {
            return v;
        }
    }
}

async fn http_post(addr: std::net::SocketAddr, path: &str, body: &str) -> (u16, Value) {
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    http(addr, &req).await
}

async fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    http(addr, &req).await
}

async fn http(addr: std::net::SocketAddr, req: &str) -> (u16, Value) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("{}");
    (
        status,
        serde_json::from_str(body.trim()).unwrap_or(Value::Null),
    )
}
