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
    let rounds = replay["rounds"].as_array().expect("rounds");
    assert_eq!(rounds.len(), 1);
    assert!(
        !rounds[0]["ticks"].as_array().unwrap().is_empty(),
        "round 0 should keep its input log"
    );
}

#[tokio::test]
async fn reconnect_after_finish_is_not_a_room() {
    let addr = spawn_server(Config {
        instant: true,
        ..Config::default()
    })
    .await;

    let created: Value = http_post(addr, "/api/matches", r#"{"seed":1}"#).await.1;
    let id = created["id"].as_str().unwrap().to_string();
    let ours_token = created["ours_token"].as_str().unwrap().to_string();
    let theirs_token = created["theirs_token"].as_str().unwrap().to_string();

    let _ = tokio::join!(
        play(addr, &id, &ours_token, true),
        play(addr, &id, &theirs_token, false),
    );

    let url = format!("ws://{addr}/ws?match={id}&token={ours_token}");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let err = wait_type(&mut stream, "error").await;
    assert_eq!(err["message"].as_str(), Some("finished"), "{err}");
}

#[tokio::test]
async fn abort_stops_lockstep_before_the_round_ends() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-abort-stop-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "abortstopabortstopabortstopabortst";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 3,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: id,
            round: 0,
            path: "lib.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: FighterStats::default(),
            theirs_stats: FighterStats::default(),
        },
    )
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
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let ours_url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token=theirs-token");
    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (theirs_ws, _) = tokio_tungstenite::connect_async(&theirs_url).await.unwrap();
    let (_, mut ours_stream) = ours_ws.split();
    let (_, mut theirs_stream) = theirs_ws.split();
    let _ = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;

    assert!(
        git_fight_server::db::abort_open_match(&pool, id, "outdated")
            .await
            .unwrap()
    );

    let err = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let msg = ours_stream.next().await.unwrap().unwrap();
            let Message::Text(text) = msg else {
                continue;
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            match v["type"].as_str() {
                Some("end") => panic!("aborted match must not send End {v}"),
                Some("error") => return v,
                _ => {}
            }
        }
    })
    .await
    .expect("aborted match should stop the room without waiting for the round timer");
    assert_eq!(err["message"].as_str(), Some("outdated"), "{err}");
    let row = git_fight_server::db::get_match(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "aborted");
    let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert!(hunks[0].winner.is_none());
}

#[tokio::test]
async fn resume_after_stored_round_result_starts_next_round() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-resume-result-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "resume-ko1resume-ko1resume-ko1resu";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 11,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    let seed = git_fight_server::protocol::round_seed(11, 0);
    let mut sim = FightState::new(seed, FighterStats::default(), FighterStats::default());
    let mut tick = 0u32;
    while sim.result.is_none() {
        let ours = if tick.is_multiple_of(14) { 1 } else { 0 };
        sim.step(Input::from_u8(ours), Input::from_u8(0));
        git_fight_server::db::insert_input(&pool, id, 0, tick, ours, 0)
            .await
            .unwrap();
        tick = tick.saturating_add(1);
        assert!(tick < 20_000, "round 0 never ended");
    }
    git_fight_server::db::set_status(&pool, id, "in_progress", true, false, None, None)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            pool,
            Config {
                instant: true,
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let mut round = None;
    for _ in 0..8 {
        let hello = wait_type(&mut stream, "hello").await;
        round = hello["round"].as_u64();
        if round == Some(1) {
            break;
        }
    }
    assert_eq!(
        round,
        Some(1),
        "stored KO must resume into round 1, not exit"
    );
}

#[tokio::test]
async fn scored_rounds_resume_finishes_without_replaying() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-scored-all-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "scoredallscoredallscoredallscore";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 11,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
        git_fight_server::db::set_hunk_winner(&pool, id, round, "ours")
            .await
            .unwrap();
    }
    git_fight_server::db::set_status(&pool, id, "in_progress", true, false, None, None)
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
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let mut status = String::new();
    for _ in 0..50 {
        let row = git_fight_server::db::get_match(&pool, id)
            .await
            .unwrap()
            .unwrap();
        status = row.status;
        if status == "finished" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, "finished");
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let err = wait_type(&mut stream, "error").await;
    assert_eq!(err["message"].as_str(), Some("finished"), "{err}");
}

#[tokio::test]
async fn lag_holds_outbound_hello() {
    let addr = spawn_server(Config {
        lag: Duration::from_millis(80),
        instant: true,
        ..Config::default()
    })
    .await;
    let created: Value = http_post(addr, "/api/matches", r#"{"seed":1}"#).await.1;
    let id = created["id"].as_str().unwrap();
    let token = created["ours_token"].as_str().unwrap();
    let url = format!("ws://{addr}/ws?match={id}&token={token}");
    let started = tokio::time::Instant::now();
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(hello["type"].as_str(), Some("hello"));
    assert!(
        started.elapsed() >= Duration::from_millis(60),
        "Hello arrived in {:?} without --lag-ms hold",
        started.elapsed()
    );
}

#[tokio::test]
async fn closed_match_socket_sends_error() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-closed-ws-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    git_fight_server::db::insert_match(&pool, "exp1", 1, 3, "o", "t", 3600)
        .await
        .unwrap();
    git_fight_server::db::set_status(&pool, "exp1", "expired", false, true, None, Some("expired"))
        .await
        .unwrap();
    git_fight_server::db::insert_match(&pool, "ab1", 1, 3, "o", "t", 3600)
        .await
        .unwrap();
    git_fight_server::db::set_status(&pool, "ab1", "aborted", false, true, None, Some("too_many"))
        .await
        .unwrap();
    git_fight_server::db::insert_match(&pool, "old1", 1, 3, "o", "t", 3600)
        .await
        .unwrap();
    git_fight_server::db::set_status(
        &pool,
        "old1",
        "aborted",
        false,
        true,
        None,
        Some("outdated"),
    )
    .await
    .unwrap();
    git_fight_server::db::insert_match(&pool, "fin1", 1, 3, "o", "t", 3600)
        .await
        .unwrap();
    git_fight_server::db::set_status(
        &pool,
        "fin1",
        "finished",
        true,
        true,
        Some("deadbeef"),
        None,
    )
    .await
    .unwrap();
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: "prep1".into(),
            seed: 1,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: Some("alice".into()),
            theirs_login: None,
            ours_token: "ours".into(),
            theirs_token: "theirs".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: "a".into(),
            pr_base_sha: "b".into(),
        },
    )
    .await
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(listener, serve_pool, Config::default())
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    for (id, want) in [
        ("exp1", "expired"),
        ("ab1", "aborted"),
        ("old1", "outdated"),
        ("fin1", "finished"),
        ("missing", "not found"),
        ("prep1", "preparing"),
    ] {
        let url = format!("ws://{addr}/ws?match={id}");
        let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
        let (_, mut stream) = ws.split();
        let err = wait_type(&mut stream, "error").await;
        assert_eq!(err["message"].as_str(), Some(want), "{err}");
    }

    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: "prep1",
            round: 0,
            path: "a.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: FighterStats::default(),
            theirs_stats: FighterStats::default(),
        },
    )
    .await
    .unwrap();
    let url = format!("ws://{addr}/ws?match=prep1");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(hello["type"].as_str(), Some("hello"), "{hello}");
    assert_eq!(hello["path"].as_str(), Some("a.rs"));
}

#[tokio::test]
async fn cpu_lockstep_hashes_agree_with_client() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-cpu-hash-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "cpuhash00cpuhash00cpuhash00cpuha";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 9,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: id,
            round: 0,
            path: "a.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: FighterStats::default(),
            theirs_stats: FighterStats::default(),
        },
    )
    .await
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            pool,
            Config {
                instant: true,
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let server = play(addr, id, "ours-token", true).await;
    let replay: Value = http_get(addr, &format!("/api/replays/{id}")).await.1;
    let hash = replay["final_hash"].as_str().expect("finished replay");
    let expected = format!("{:08x}{:08x}", server >> 32, server as u32);
    assert_eq!(hash, expected, "server hash {hash} != client {expected}");
}

#[tokio::test]
async fn hello_includes_stored_fighter_stats() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-hello-stats-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: "cafe0007cafe0007cafe0007cafe0007".into(),
            seed: 9,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: "cafe0007cafe0007cafe0007cafe0007",
            round: 0,
            path: "lib.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: None,
            theirs_name: Some("bob"),
            ours_stats: FighterStats {
                hp: 120,
                armor: true,
                special: true,
            },
            theirs_stats: FighterStats {
                hp: 80,
                armor: false,
                special: false,
            },
        },
    )
    .await
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(listener, pool, Config::default())
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let url = format!("ws://{addr}/ws?match=cafe0007cafe0007cafe0007cafe0007&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(hello["ours_hp"].as_i64(), Some(120));
    assert_eq!(hello["ours_armor"].as_bool(), Some(true));
    assert_eq!(hello["ours_special"].as_bool(), Some(true));
    assert_eq!(hello["theirs_hp"].as_i64(), Some(80));
    assert_eq!(hello["theirs_armor"].as_bool(), Some(false));
    assert_eq!(hello["theirs_special"].as_bool(), Some(false));
    assert_eq!(hello["you_are"].as_str(), Some(""));
    assert_eq!(hello["your_role"].as_str(), Some("ours"));
    assert_eq!(hello["path"].as_str(), Some("lib.rs"));
    assert_eq!(hello["hunk_index"].as_u64(), Some(0));
}

#[tokio::test]
async fn reconnect_resumes_later_round_from_stored_inputs() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir =
        std::env::temp_dir().join(format!("gf-resume-{}-{}", std::process::id(), uuid_like()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: "resume0001resume0001resume0001re".into(),
            seed: 11,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    let id = "resume0001resume0001resume0001re";
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    git_fight_server::db::set_hunk_winner(&pool, id, 0, "ours")
        .await
        .unwrap();
    git_fight_server::db::insert_input(&pool, id, 1, 0, 1, 0)
        .await
        .unwrap();
    git_fight_server::db::insert_input(&pool, id, 1, 1, 0, 1)
        .await
        .unwrap();
    git_fight_server::db::insert_input(&pool, id, 1, 2, 0, 0)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(listener, pool, Config::default())
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(hello["round"].as_u64(), Some(1));
    assert_eq!(hello["total_rounds"].as_u64(), Some(2));
    assert_eq!(hello["confirmed_tick"].as_i64(), Some(2));
    let snap = wait_type(&mut stream, "snapshot").await;
    assert_eq!(snap["round"].as_u64(), Some(1));
    assert_eq!(snap["confirmed_tick"].as_i64(), Some(2));
    assert_eq!(
        snap["ticks"],
        serde_json::json!([[0, 1, 0], [1, 0, 1], [2, 0, 0]])
    );
    let seed_lo = snap["seed_lo"].as_u64().unwrap() as u32;
    let seed_hi = snap["seed_hi"].as_u64().unwrap() as u32;
    let seed = (u64::from(seed_hi) << 32) | u64::from(seed_lo);
    let mut sim = FightState::new(seed, FighterStats::default(), FighterStats::default());
    for pair in snap["ticks"].as_array().unwrap() {
        sim.step(
            Input::from_u8(pair[1].as_u64().unwrap() as u8),
            Input::from_u8(pair[2].as_u64().unwrap() as u8),
        );
    }
    assert_eq!(sim.tick, 3);
}

#[tokio::test]
async fn multi_round_replay_keeps_every_round() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-replay-rounds-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "replay0001replay0001replay0001re";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 7,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
        git_fight_server::db::insert_input(&pool, id, round as u32, 0, 1, 0)
            .await
            .unwrap();
        git_fight_server::db::insert_input(&pool, id, round as u32, 1, 0, 1)
            .await
            .unwrap();
    }
    git_fight_server::db::set_status(&pool, id, "finished", true, true, Some("aabbccdd"), None)
        .await
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(listener, pool, Config::default())
            .await
            .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let replay: Value = http_get(addr, &format!("/api/replays/{id}")).await.1;
    assert_eq!(replay["total_rounds"].as_u64(), Some(2));
    let rounds = replay["rounds"].as_array().expect("rounds");
    assert_eq!(rounds.len(), 2);
    assert_eq!(rounds[0]["ticks"], serde_json::json!([[1, 0], [0, 1]]));
    assert_eq!(rounds[1]["ticks"], serde_json::json!([[1, 0], [0, 1]]));
    assert_ne!(rounds[0]["seed"], rounds[1]["seed"]);
    assert_eq!(rounds[0]["seed"].as_str(), Some("7"));
    assert_eq!(rounds[1]["seed"].as_str(), Some("14"));
    assert_eq!(rounds[0]["path"].as_str(), Some("lib.rs"));
    assert_eq!(rounds[1]["path"].as_str(), Some("lib.rs"));
    assert_eq!(rounds[0]["hunk_index"].as_u64(), Some(0));
    assert_eq!(rounds[1]["hunk_index"].as_u64(), Some(1));
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
async fn in_progress_match_expires_without_result() {
    let addr = spawn_server(Config {
        instant: true,
        expire_secs: 1,
        ..Config::default()
    })
    .await;
    let created: Value = http_post(addr, "/api/matches", r#"{"seed":9}"#).await.1;
    let id = created["id"].as_str().unwrap().to_string();
    let ours_token = created["ours_token"].as_str().unwrap().to_string();
    let theirs_token = created["theirs_token"].as_str().unwrap().to_string();
    let ours_url = format!("ws://{addr}/ws?match={id}&token={ours_token}");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token={theirs_token}");
    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (theirs_ws, _) = tokio_tungstenite::connect_async(&theirs_url).await.unwrap();
    let (_, mut ours_stream) = ours_ws.split();
    let (_, mut theirs_stream) = theirs_ws.split();
    let _ = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;
    let err = tokio::time::timeout(Duration::from_secs(5), wait_type(&mut ours_stream, "error"))
        .await
        .expect("in-progress match should expire");
    assert_eq!(err["message"].as_str(), Some("expired"), "{err}");
    let info: Value = http_get(addr, &format!("/api/matches/{id}")).await.1;
    assert_eq!(info["status"].as_str(), Some("expired"), "{info}");
    assert_eq!(info["abort_reason"].as_str(), Some("expired"), "{info}");
    let (replay_status, _) = http_get(addr, &format!("/api/replays/{id}")).await;
    assert_eq!(replay_status, 404);
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

#[tokio::test]
async fn forfeit_round_then_return_plays_later_round() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-forfeit-return-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "forfeit01forfeit01forfeit01forfe";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 5,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            pool,
            Config {
                instant: true,
                disconnect: Duration::from_millis(80),
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let ours_url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token=theirs-token");
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

    let mut round1_hello = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let end0 = loop {
        let msg = tokio::time::timeout_at(deadline, theirs_stream.next())
            .await
            .expect("timeout waiting for forfeit")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(0), "first end should be round 0");
                break v;
            }
            Some("hello") => round1_hello = Some(v),
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    };
    assert_eq!(
        end0["result"].as_i64(),
        Some(1),
        "ours disconnect => theirs"
    );
    assert_eq!(end0["match_over"].as_bool(), Some(false));

    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (mut ours_sink, mut ours_stream) = ours_ws.split();
    let ours_hello = wait_type(&mut ours_stream, "hello").await;
    assert_eq!(ours_hello["round"].as_u64(), Some(1));
    assert_eq!(ours_hello["your_role"].as_str(), Some("ours"));
    let theirs_hello = match round1_hello {
        Some(v) => v,
        None => wait_type(&mut theirs_stream, "hello").await,
    };
    assert_eq!(theirs_hello["round"].as_u64(), Some(1));

    let mut next_send = 0u32;
    let mut confirmed: i32 = -1;
    let play_deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    let end1 = loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let msg = format!(r#"{{"type":"input","tick":{next_send},"buttons":0}}"#);
            ours_sink
                .send(Message::Text(msg.clone().into()))
                .await
                .unwrap();
            theirs_sink.send(Message::Text(msg.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }
        let msg = tokio::time::timeout_at(play_deadline, theirs_stream.next())
            .await
            .expect("timeout waiting for later round")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("tick") => confirmed = v["n"].as_i64().unwrap_or(0) as i32,
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(1), "second end should be round 1");
                break v;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    };
    assert_eq!(end1["match_over"].as_bool(), Some(true));

    let replay: Value = http_get(addr, &format!("/api/replays/{id}")).await.1;
    let rounds = replay["rounds"].as_array().expect("rounds");
    assert_eq!(rounds.len(), 2);
    assert!(
        !rounds[1]["ticks"].as_array().unwrap().is_empty(),
        "later round should have been played"
    );
}

#[tokio::test]
async fn previous_round_input_does_not_steer_the_next_round() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-stale-round-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "staleroundstaleroundstaleroundsta";
    git_fight_server::db::insert_full_match(
        &pool,
        &NewMatch {
            id: id.into(),
            seed: 9,
            delay: 3,
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "ours-token".into(),
            theirs_token: "theirs-token".into(),
            expire_secs: 3600,
            installation_id: None,
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
        },
    )
    .await
    .unwrap();
    for round in 0..2 {
        git_fight_server::db::insert_hunk(
            &pool,
            &NewHunk {
                match_id: id,
                round,
                path: "lib.rs",
                hunk_index: round,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            pool,
            Config {
                instant: true,
                ..Config::default()
            },
        )
        .await
        .unwrap();
    });
    for _ in 0..80 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let ours_url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token=theirs-token");
    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (theirs_ws, _) = tokio_tungstenite::connect_async(&theirs_url).await.unwrap();
    let (mut ours_sink, mut ours_stream) = ours_ws.split();
    let (mut theirs_sink, mut theirs_stream) = theirs_ws.split();
    let _ = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;

    let mut next_send = 0u32;
    let mut confirmed: i32 = -1;
    let mut round1_hello = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let buttons = if next_send.is_multiple_of(14) { 1 } else { 0 };
            let msg =
                format!(r#"{{"type":"input","tick":{next_send},"buttons":{buttons},"round":0}}"#);
            ours_sink
                .send(Message::Text(msg.clone().into()))
                .await
                .unwrap();
            theirs_sink.send(Message::Text(msg.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }
        let msg = tokio::time::timeout_at(deadline, theirs_stream.next())
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
            Some("hello") if v["round"].as_u64() == Some(1) => round1_hello = Some(v),
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(0));
                assert_eq!(v["match_over"].as_bool(), Some(false));
                break;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    }
    if round1_hello.is_none() {
        let hello = wait_type(&mut theirs_stream, "hello").await;
        assert_eq!(hello["round"].as_u64(), Some(1));
    }
    let ours_hello = wait_type(&mut ours_stream, "hello").await;
    assert_eq!(ours_hello["round"].as_u64(), Some(1));

    // Leftover special from the KO round must not land on round 1 tick 5.
    let stale = r#"{"type":"input","tick":5,"buttons":4,"round":0}"#;
    ours_sink.send(Message::Text(stale.into())).await.unwrap();
    theirs_sink.send(Message::Text(stale.into())).await.unwrap();

    next_send = 0;
    confirmed = -1;
    loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let msg = format!(r#"{{"type":"input","tick":{next_send},"buttons":0,"round":1}}"#);
            ours_sink
                .send(Message::Text(msg.clone().into()))
                .await
                .unwrap();
            theirs_sink.send(Message::Text(msg.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }
        let msg = tokio::time::timeout_at(deadline, theirs_stream.next())
            .await
            .expect("timeout waiting for round 1")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("tick") => confirmed = v["n"].as_i64().unwrap_or(0) as i32,
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(1));
                assert_eq!(v["match_over"].as_bool(), Some(true));
                break;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    }

    let replay: Value = http_get(addr, &format!("/api/replays/{id}")).await.1;
    let ticks = replay["rounds"][1]["ticks"]
        .as_array()
        .expect("round 1 ticks");
    assert!(ticks.len() > 5, "round 1 should have reached tick 5");
    assert_eq!(
        ticks[5],
        serde_json::json!([0, 0]),
        "stale round-0 special must not confirm as round 1 tick 5"
    );
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
    let ours_stats = FighterStats::clamped(
        hello["ours_hp"].as_i64().unwrap_or(100) as i32,
        hello["ours_armor"].as_bool().unwrap_or(false),
        hello["ours_special"].as_bool().unwrap_or(false),
    );
    let theirs_stats = FighterStats::clamped(
        hello["theirs_hp"].as_i64().unwrap_or(100) as i32,
        hello["theirs_armor"].as_bool().unwrap_or(false),
        hello["theirs_special"].as_bool().unwrap_or(false),
    );
    let mut sim = FightState::new(seed, ours_stats, theirs_stats);
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
            if sink.send(Message::Text(msg.into())).await.is_err() {
                break;
            }
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
            Some("hash") => {
                let n = v["n"].as_u64().unwrap() as u32;
                if n == sim.tick {
                    let hi = v["hi"].as_u64().unwrap();
                    let lo = v["lo"].as_u64().unwrap();
                    let server = (hi << 32) | lo;
                    assert_eq!(sim.state_hash(), server, "Hash desync at tick {n}");
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
