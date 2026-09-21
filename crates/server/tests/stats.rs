use futures_util::{SinkExt, StreamExt};
use git_fight_server::db::{NewHunk, NewMatch, PlayerStat};
use git_fight_server::{Auth, Config};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;

const KEY: &[u8] = b"session-key-session-key-session!";
const MATCH_ID: &str = "cafe0006cafe0006cafe0006cafe0006";

async fn pool() -> sqlx::SqlitePool {
    git_fight_server::db_connect("sqlite::memory:")
        .await
        .unwrap()
}

async fn seed(
    pool: &sqlx::SqlitePool,
    id: &str,
    ours: Option<&str>,
    theirs: Option<&str>,
    owner: &str,
    repo: &str,
) {
    git_fight_server::db::insert_full_match(
        pool,
        &NewMatch {
            id: id.into(),
            seed: 1,
            delay: 3,
            ours_name: ours.unwrap_or("ours").into(),
            theirs_name: theirs.unwrap_or("theirs").into(),
            ours_kind: "github".into(),
            theirs_kind: if theirs.is_some() { "github" } else { "cpu" }.into(),
            ours_login: ours.map(str::to_string),
            theirs_login: theirs.map(str::to_string),
            ours_token: "o".into(),
            theirs_token: "t".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: owner.into(),
            repo: repo.into(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        pool,
        &NewHunk {
            match_id: id,
            round: 0,
            path: "lib.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: theirs,
            theirs_name: theirs,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn ko_win_updates_wins_losses_kos_and_conflicts_caused() {
    let pool = pool().await;
    seed(&pool, "m1", Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::record_round(&pool, "m1", 0, "ours", true)
        .await
        .unwrap();
    let board = git_fight_server::db::list_player_stats(&pool, "acme", "box")
        .await
        .unwrap();
    assert_eq!(
        board,
        vec![
            PlayerStat {
                github_login: "alice".into(),
                wins: 1,
                losses: 0,
                kos: 1,
                conflicts_caused: 0,
            },
            PlayerStat {
                github_login: "bob".into(),
                wins: 0,
                losses: 1,
                kos: 0,
                conflicts_caused: 1,
            },
        ]
    );
}

#[tokio::test]
async fn draw_skips_wins_but_counts_conflicts_caused() {
    let pool = pool().await;
    seed(&pool, "m2", Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::record_round(&pool, "m2", 0, "draw", false)
        .await
        .unwrap();
    let alice = git_fight_server::db::get_player_stats(&pool, "acme", "box", "alice")
        .await
        .unwrap();
    let bob = git_fight_server::db::get_player_stats(&pool, "acme", "box", "bob")
        .await
        .unwrap();
    assert_eq!(alice.wins, 0);
    assert_eq!(alice.losses, 0);
    assert_eq!(alice.kos, 0);
    assert_eq!(bob.conflicts_caused, 1);
    assert_eq!(bob.wins, 0);
}

#[tokio::test]
async fn forfeit_is_a_win_without_a_ko() {
    let pool = pool().await;
    seed(&pool, "m3", Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::record_round(&pool, "m3", 0, "forfeit_ours", false)
        .await
        .unwrap();
    let alice = git_fight_server::db::get_player_stats(&pool, "acme", "box", "alice")
        .await
        .unwrap();
    let bob = git_fight_server::db::get_player_stats(&pool, "acme", "box", "bob")
        .await
        .unwrap();
    assert_eq!(alice.losses, 1);
    assert_eq!(alice.kos, 0);
    assert_eq!(bob.wins, 1);
    assert_eq!(bob.kos, 0);
}

#[tokio::test]
async fn timeout_win_is_not_a_ko() {
    let pool = pool().await;
    seed(&pool, "m4", Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::record_round(&pool, "m4", 0, "theirs", false)
        .await
        .unwrap();
    let bob = git_fight_server::db::get_player_stats(&pool, "acme", "box", "bob")
        .await
        .unwrap();
    assert_eq!(bob.wins, 1);
    assert_eq!(bob.kos, 0);
}

#[tokio::test]
async fn local_match_without_repo_does_not_record() {
    let pool = pool().await;
    seed(&pool, "m5", Some("alice"), Some("bob"), "", "").await;
    git_fight_server::record_round(&pool, "m5", 0, "ours", true)
        .await
        .unwrap();
    let board = git_fight_server::db::list_player_stats(&pool, "acme", "box")
        .await
        .unwrap();
    assert!(board.is_empty());
}

#[tokio::test]
async fn cpu_side_without_login_is_skipped() {
    let pool = pool().await;
    seed(&pool, "m6", Some("alice"), None, "acme", "box").await;
    git_fight_server::record_round(&pool, "m6", 0, "ours", true)
        .await
        .unwrap();
    let board = git_fight_server::db::list_player_stats(&pool, "acme", "box")
        .await
        .unwrap();
    assert_eq!(board.len(), 1);
    assert_eq!(board[0].github_login, "alice");
    assert_eq!(board[0].wins, 1);
    assert_eq!(board[0].conflicts_caused, 0);
}

#[tokio::test]
async fn leaderboard_and_badge_http() {
    let dir = std::env::temp_dir().join(format!(
        "gf-lb-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    seed(&pool, "m7", Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::record_round(&pool, "m7", 0, "ours", true)
        .await
        .unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(listener, pool, Config::default())
            .await
            .unwrap();
    });
    wait_up(addr).await;

    let (status, ctype, body) = http(
        addr,
        "GET /acme/box/leaderboard HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(ctype.contains("application/json"), "{ctype}");
    let v: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["players"][0]["login"], "alice");
    assert_eq!(v["players"][0]["wins"], 1);
    assert_eq!(v["players"][1]["login"], "bob");
    assert_eq!(v["players"][1]["conflicts_caused"], 1);

    let (status, ctype, body) = http(
        addr,
        "GET /acme/box/leaderboard HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(ctype.contains("text/html"), "{ctype}");
    assert!(body.contains("alice"));
    assert!(body.contains("bob"));
    assert!(body.contains("acme/box"));

    let (status, ctype, body) = http(
        addr,
        "GET /badge/acme/box/alice HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(ctype.contains("image/svg+xml"), "{ctype}");
    assert!(body.contains("1 wins"));
    assert!(body.contains("#FF4A1C"));
    assert!(body.contains("#0A0A0B"));

    let (status, _, body) = http(
        addr,
        "GET /badge/acme/box/nobody HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    assert!(body.contains("0 wins"));

    let (status, _, _) = http(
        addr,
        "GET /badge/acme/box/alice..x HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 400);

    let (status, _, body) = http(
        addr,
        "GET /other/repo/leaderboard HTTP/1.1\r\nHost: localhost\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
    )
    .await;
    assert_eq!(status, 200);
    let v: Value = serde_json::from_str(&body).unwrap();
    assert!(v["players"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn live_match_records_stats() {
    let dir = std::env::temp_dir().join(format!(
        "gf-live-stats-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    seed(&pool, MATCH_ID, Some("alice"), Some("bob"), "acme", "box").await;
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            serve_pool,
            Config {
                instant: true,
                auth: Auth {
                    session_key: KEY.to_vec(),
                    public_url: "http://fight.test".into(),
                },
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    wait_up(addr).await;

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
            Some("end") => break,
            Some("error") => panic!("{}", v["message"]),
            _ => {}
        }
    }

    let mut board = Vec::new();
    for _ in 0..40 {
        board = git_fight_server::db::list_player_stats(&pool, "acme", "box")
            .await
            .unwrap();
        if !board.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        board
            .iter()
            .any(|p| p.github_login == "alice" && (p.wins + p.losses) == 1),
        "alice missing a decided round: {board:?}"
    );
    let bob = board.iter().find(|p| p.github_login == "bob").unwrap();
    assert_eq!(bob.conflicts_caused, 1, "{board:?}");
}

async fn wait_up(addr: std::net::SocketAddr) {
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn http(addr: std::net::SocketAddr, req: &str) -> (u16, String, String) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await.unwrap();
    let text = String::from_utf8_lossy(&buf);
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let ctype = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            if k.eq_ignore_ascii_case("content-type") {
                Some(v.trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();
    (status, ctype, body.trim().to_string())
}
