use futures_util::{SinkExt, StreamExt};
use git_fight_core::{FightState, FighterStats, Input};
use git_fight_server::Config;
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
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
async fn aborted_scored_last_round_sends_error_not_match_over() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-abort-scored");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "abortscoredlastround000000000000";
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
    assert!(
        git_fight_server::db::set_hunk_winner(&pool, id, 0, "ours", true)
            .await
            .unwrap()
    );
    git_fight_server::db::set_status(&pool, id, "in_progress", true, false, None, None)
        .await
        .unwrap();
    assert!(
        git_fight_server::db::abort_open_match(&pool, id, "outdated")
            .await
            .unwrap()
    );
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
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let err = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let msg = stream.next().await.unwrap().unwrap();
            let Message::Text(text) = msg else {
                continue;
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            match v["type"].as_str() {
                Some("end") => panic!("aborted last round must not send End {v}"),
                Some("error") => return v,
                _ => {}
            }
        }
    })
    .await
    .expect("aborted scored match should Error, not match_over");
    assert_eq!(err["message"].as_str(), Some("outdated"), "{err}");
    let row = git_fight_server::db::get_match(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "aborted");
    assert!(row.final_hash.is_none());
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
async fn replayed_ko_is_scored_before_first_join() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-boot-finish");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "bootfinishbootfinishbootfinishboo";
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
    let serve_pool = pool.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    let mut winner = None;
    for _ in 0..50 {
        let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
        winner = hunks
            .into_iter()
            .find(|h| h.round_index == 0)
            .and_then(|h| h.winner);
        if winner.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        winner.is_some(),
        "boot must persist the replayed KO before anyone joins"
    );
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(
        hello["round"].as_u64(),
        Some(1),
        "first Join must land on the next conflict, not the decided round"
    );
}

#[tokio::test]
async fn pending_forfeit_is_applied_before_first_join() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-boot-forfeit");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "bootforfeitbootforfeitbootforfeit";
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
    git_fight_server::db::set_status(&pool, id, "in_progress", true, false, None, None)
        .await
        .unwrap();
    assert!(
        git_fight_server::db::set_pending_forfeit(&pool, id, 0, "forfeit_ours")
            .await
            .unwrap()
    );
    let serve_pool = pool.clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
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
    let mut winner = None;
    for _ in 0..50 {
        let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
        winner = hunks
            .into_iter()
            .find(|h| h.round_index == 0)
            .and_then(|h| h.winner);
        if winner.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        winner.as_deref(),
        Some("forfeit_ours"),
        "boot must persist a latched disconnect forfeit before anyone joins"
    );
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let hello = wait_type(&mut stream, "hello").await;
    assert_eq!(
        hello["round"].as_u64(),
        Some(1),
        "first Join must land on the next conflict after the latched forfeit"
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
        let tag = if round == 1 { "forfeit_ours" } else { "ours" };
        git_fight_server::db::set_hunk_winner(&pool, id, round, tag, false)
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
async fn scored_all_past_deadline_finishes_not_expires() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-scored-deadline");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "scoredalldeadline00000000000000";
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
            expire_secs: 0,
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
    git_fight_server::db::set_hunk_winner(&pool, id, 0, "forfeit_ours", false)
        .await
        .unwrap();
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
    assert_eq!(
        status, "finished",
        "a fully scored fight past expires_at must finish, not expire"
    );
}

#[tokio::test]
async fn scored_all_without_terminal_sim_does_not_fake_hash() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-scored-nohash");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "scoredallnohash0000000000000000";
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
    git_fight_server::db::set_hunk_winner(&pool, id, 0, "ours", false)
        .await
        .unwrap();
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
    let url = format!("ws://{addr}/ws?match={id}&token=ours-token");
    let (ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let (_, mut stream) = ws.split();
    let err = wait_type(&mut stream, "error").await;
    assert_eq!(
        err["message"].as_str(),
        Some("preparing"),
        "a scored row with no terminal sim must not write a fake final_hash"
    );
    let closed = tokio::time::timeout(Duration::from_secs(2), async {
        while stream.next().await.is_some() {}
    })
    .await;
    assert!(
        closed.is_ok(),
        "preparing must close the socket so the canvas reconnects"
    );
    let row = git_fight_server::db::get_match(&pool, id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(row.status, "in_progress");
    assert!(row.final_hash.is_none());
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
    git_fight_server::db::set_hunk_winner(&pool, id, 0, "ours", false)
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
    let dir = git_fight_server::test_tmp_dir("git-fight-exp");
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
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            serve_pool,
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
    let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert_eq!(
        hunks[0].winner.as_deref(),
        Some("forfeit_ours"),
        "disconnect forfeit must be stored before End so resume cannot replay a KO pick"
    );

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
async fn later_round_disconnect_forfeits_after_prior_latch() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-forfeit-later");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "forfeit2forfeit2forfeit2forfeit2";
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
    git_fight_server::db::set_status(&pool, id, "in_progress", true, false, None, None)
        .await
        .unwrap();
    assert!(
        git_fight_server::db::set_pending_forfeit(&pool, id, 0, "forfeit_ours")
            .await
            .unwrap()
    );
    git_fight_server::db::set_hunk_winner(&pool, id, 0, "forfeit_ours", false)
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
    let (theirs_sink, mut theirs_stream) = theirs_ws.split();
    let ours_hello = wait_type(&mut ours_stream, "hello").await;
    let theirs_hello = wait_type(&mut theirs_stream, "hello").await;
    assert_eq!(ours_hello["round"].as_u64(), Some(1));
    assert_eq!(theirs_hello["round"].as_u64(), Some(1));
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
    let end1 = loop {
        let msg = tokio::time::timeout_at(deadline, theirs_stream.next())
            .await
            .expect("timeout waiting for later-round forfeit")
            .expect("ws closed")
            .unwrap();
        let Message::Text(text) = msg else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        match v["type"].as_str() {
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(1), "{v}");
                break v;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    };
    assert_eq!(end1["result"].as_i64(), Some(1));
    assert_eq!(end1["match_over"].as_bool(), Some(true));
    let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert_eq!(hunks[0].winner.as_deref(), Some("forfeit_ours"));
    assert_eq!(
        hunks[1].winner.as_deref(),
        Some("forfeit_ours"),
        "a leftover round-0 latch must not block a later-round disconnect forfeit"
    );
}

#[tokio::test]
async fn later_round_author_is_not_forfeited_before_they_join() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = git_fight_server::test_tmp_dir("gf-later-author");
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "laterauth01laterauth01laterauth0";
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
            ours_login: Some("alice".into()),
            theirs_login: Some("bob".into()),
            ours_token: String::new(),
            theirs_token: String::new(),
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
            theirs_login: Some("bob"),
            theirs_name: Some("bob"),
            ours_stats: FighterStats::default(),
            theirs_stats: FighterStats::default(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_hunk(
        &pool,
        &NewHunk {
            match_id: id,
            round: 1,
            path: "b.rs",
            hunk_index: 0,
            ours: b"a",
            theirs: b"b",
            base: b"c",
            theirs_login: Some("carol"),
            theirs_name: Some("carol"),
            ours_stats: FighterStats::default(),
            theirs_stats: FighterStats::default(),
        },
    )
    .await
    .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-bob", 2, "bob")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-carol", 3, "carol")
        .await
        .unwrap();
    let key = git_fight_server::Auth::default().session_key;
    let alice_c = git_fight_server::sign_session(&key, "sid-alice");
    let bob_c = git_fight_server::sign_session(&key, "sid-bob");
    let carol_c = git_fight_server::sign_session(&key, "sid-carol");

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve_pool = pool.clone();
    tokio::spawn(async move {
        git_fight_server::serve(
            listener,
            serve_pool,
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

    let (mut alice_sink, mut alice_stream) = connect_cookie(addr, id, &alice_c).await;
    let (mut bob_sink, mut bob_stream) = connect_cookie(addr, id, &bob_c).await;
    let alice_h = wait_type(&mut alice_stream, "hello").await;
    let bob_h = wait_type(&mut bob_stream, "hello").await;
    assert_eq!(alice_h["your_role"].as_str(), Some("ours"), "{alice_h}");
    assert_eq!(bob_h["your_role"].as_str(), Some("theirs"), "{bob_h}");

    let mut next_send = 0u32;
    let mut confirmed: i32 = -1;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let punch = format!(r#"{{"type":"input","tick":{next_send},"buttons":2,"round":0}}"#);
            let idle = format!(r#"{{"type":"input","tick":{next_send},"buttons":0,"round":0}}"#);
            alice_sink.send(Message::Text(punch.into())).await.unwrap();
            bob_sink.send(Message::Text(idle.into())).await.unwrap();
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
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(0));
                assert_eq!(v["match_over"].as_bool(), Some(false));
                break;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    }

    tokio::time::sleep(Duration::from_millis(250)).await;
    while let Ok(Some(msg)) =
        tokio::time::timeout(Duration::from_millis(20), alice_stream.next()).await
    {
        let Message::Text(text) = msg.unwrap() else {
            continue;
        };
        let v: Value = serde_json::from_str(&text).unwrap();
        if v["type"].as_str() == Some("end") && v["round"].as_u64() == Some(1) {
            panic!("carol was forfeited before she joined: {v}");
        }
    }
    let hunks = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert_eq!(hunks.len(), 2);
    assert!(hunks[0].winner.is_some(), "round 0 should have a winner");
    assert_ne!(
        hunks[0].winner.as_deref(),
        Some("forfeit_ours"),
        "{:?}",
        hunks[0].winner
    );
    assert!(
        hunks[1].winner.is_none(),
        "later-round author must not be auto-forfeited: {:?}",
        hunks[1].winner
    );

    let (mut carol_sink, mut carol_stream) = connect_cookie(addr, id, &carol_c).await;
    let carol_h = wait_type(&mut carol_stream, "hello").await;
    assert_eq!(carol_h["your_role"].as_str(), Some("theirs"), "{carol_h}");
    assert_eq!(carol_h["round"].as_u64(), Some(1), "{carol_h}");

    next_send = 0;
    confirmed = -1;
    let play = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let horizon = u32::try_from(confirmed.saturating_add(1)).unwrap_or(0) + 24;
        while next_send <= horizon {
            let punch = format!(r#"{{"type":"input","tick":{next_send},"buttons":2,"round":1}}"#);
            let idle = format!(r#"{{"type":"input","tick":{next_send},"buttons":0,"round":1}}"#);
            alice_sink.send(Message::Text(punch.into())).await.unwrap();
            carol_sink.send(Message::Text(idle.into())).await.unwrap();
            next_send = next_send.saturating_add(1);
        }
        let msg = tokio::time::timeout_at(play, alice_stream.next())
            .await
            .expect("timeout waiting for carol's round")
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
    let done = git_fight_server::db::list_hunks(&pool, id).await.unwrap();
    assert_eq!(
        done[1].winner.as_deref(),
        Some("ours"),
        "{:?}",
        done[1].winner
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

#[tokio::test]
async fn omitted_round_on_github_match_does_not_steer_next_conflict() {
    use git_fight_server::db::{NewHunk, NewMatch};
    let dir = std::env::temp_dir().join(format!(
        "gf-omit-round-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
    let id = "omitround01omitround01omitround0";
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
            ours_login: Some("alice".into()),
            theirs_login: Some("bob".into()),
            ours_token: String::new(),
            theirs_token: String::new(),
            expire_secs: 3600,
            installation_id: Some(1),
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            pr_base_sha: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
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
                theirs_login: Some("bob"),
                theirs_name: Some("bob"),
                ours_stats: FighterStats::default(),
                theirs_stats: FighterStats::default(),
            },
        )
        .await
        .unwrap();
    }
    git_fight_server::db::insert_session(&pool, "sid-alice", 1, "alice")
        .await
        .unwrap();
    git_fight_server::db::insert_session(&pool, "sid-bob", 2, "bob")
        .await
        .unwrap();
    let key = git_fight_server::Auth::default().session_key;
    let alice_c = git_fight_server::sign_session(&key, "sid-alice");
    let bob_c = git_fight_server::sign_session(&key, "sid-bob");

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

    let (mut ours_sink, mut ours_stream) = connect_cookie(addr, id, &alice_c).await;
    let (mut theirs_sink, mut theirs_stream) = connect_cookie(addr, id, &bob_c).await;
    let _ = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;

    let mut next_send = 0u32;
    let mut confirmed: i32 = -1;
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
            Some("end") => {
                assert_eq!(v["round"].as_u64(), Some(0));
                assert_eq!(v["match_over"].as_bool(), Some(false));
                break;
            }
            Some("error") => panic!("server error {}", v["message"]),
            _ => {}
        }
    }
    let _ = wait_type(&mut theirs_stream, "hello").await;
    let _ = wait_type(&mut ours_stream, "hello").await;

    let stale = r#"{"type":"input","tick":5,"buttons":4}"#;
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
        "omitted-round leftover special must not confirm as round 1 tick 5"
    );
}

#[tokio::test]
async fn failed_input_insert_does_not_advance_confirmed_tick() {
    let dir = std::env::temp_dir().join(format!(
        "gf-persist-first-{}-{}",
        std::process::id(),
        uuid_like()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = format!("sqlite://{}/m.db", dir.display());
    let pool = git_fight_server::db_connect(&db).await.unwrap();
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

    sqlx::query(
        "CREATE TRIGGER block_match_inputs
         BEFORE INSERT ON match_inputs
         BEGIN
           SELECT RAISE(ABORT, 'blocked');
         END",
    )
    .execute(&pool)
    .await
    .unwrap();

    let created: Value = http_post(addr, "/api/matches", r#"{"seed":1}"#).await.1;
    let id = created["id"].as_str().unwrap().to_string();
    let ours_token = created["ours_token"].as_str().unwrap().to_string();
    let theirs_token = created["theirs_token"].as_str().unwrap().to_string();

    let ours_url = format!("ws://{addr}/ws?match={id}&token={ours_token}");
    let theirs_url = format!("ws://{addr}/ws?match={id}&token={theirs_token}");
    let (ours_ws, _) = tokio_tungstenite::connect_async(&ours_url).await.unwrap();
    let (theirs_ws, _) = tokio_tungstenite::connect_async(&theirs_url).await.unwrap();
    let (mut ours_sink, mut ours_stream) = ours_ws.split();
    let (mut theirs_sink, mut theirs_stream) = theirs_ws.split();
    let ours_hello = wait_type(&mut ours_stream, "hello").await;
    let _ = wait_type(&mut theirs_stream, "hello").await;
    assert_eq!(ours_hello["confirmed_tick"].as_i64(), Some(-1));

    for tick in 0..8u32 {
        let msg = format!(r#"{{"type":"input","tick":{tick},"buttons":0}}"#);
        ours_sink
            .send(Message::Text(msg.clone().into()))
            .await
            .unwrap();
        theirs_sink.send(Message::Text(msg.into())).await.unwrap();
    }

    let stalled = tokio::time::timeout(Duration::from_millis(400), async {
        loop {
            let msg = ours_stream.next().await.unwrap().unwrap();
            let Message::Text(text) = msg else {
                continue;
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            match v["type"].as_str() {
                Some("tick") => return Some(v),
                Some("end") => panic!("must not End while match_inputs is blocked {v}"),
                Some("error") => panic!("server error {}", v["message"]),
                _ => {}
            }
        }
    })
    .await;
    assert!(
        stalled.is_err(),
        "a failed match_inputs write must not broadcast Tick: {:?}",
        stalled.ok().flatten()
    );
    assert!(
        git_fight_server::db::load_inputs(&pool, &id, 0)
            .await
            .unwrap()
            .is_empty(),
        "blocked insert must leave the log empty"
    );

    sqlx::query("DROP TRIGGER block_match_inputs")
        .execute(&pool)
        .await
        .unwrap();

    let tick = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let msg = ours_stream.next().await.unwrap().unwrap();
            let Message::Text(text) = msg else {
                continue;
            };
            let v: Value = serde_json::from_str(&text).unwrap();
            match v["type"].as_str() {
                Some("tick") => return v,
                Some("error") => panic!("server error {}", v["message"]),
                _ => {}
            }
        }
    })
    .await
    .expect("after DROP TRIGGER the room should confirm the peeked inputs");
    assert_eq!(tick["n"].as_i64(), Some(0));
    assert!(
        !git_fight_server::db::load_inputs(&pool, &id, 0)
            .await
            .unwrap()
            .is_empty(),
        "the recovered tick must be durable"
    );
}

async fn connect_cookie(
    addr: std::net::SocketAddr,
    match_id: &str,
    cookie: &str,
) -> (
    futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
        Message,
    >,
    futures_util::stream::SplitStream<
        tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<TcpStream>>,
    >,
) {
    let url = format!("ws://{addr}/ws?match={match_id}");
    let mut req = url.into_client_request().unwrap();
    req.headers_mut()
        .insert("Cookie", format!("git_fight_sid={cookie}").parse().unwrap());
    let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
    ws.split()
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
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ (std::process::id() as u64).wrapping_shl(32)
        ^ (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64)
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
