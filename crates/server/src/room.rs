use crate::db::{self, MatchRow};
use crate::protocol::{
    role_for, round_seed, split_hash, split_seed, Role, ServerMsg, DISCONNECT_SECS, INPUT_DELAY,
    INPUT_WINDOW,
};
use crate::result::{self, ResultCtx};
use chrono::{DateTime, Utc};
use git_fight_core::{FightState, FighterStats, Input, RoundResult, Side};
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

pub enum RoomEvent {
    Join {
        conn_id: u64,
        login: Option<String>,
        token: Option<String>,
        tx: mpsc::Sender<String>,
    },
    Leave {
        conn_id: u64,
    },
    Input {
        conn_id: u64,
        tick: u32,
        buttons: u8,
        theirs_buttons: Option<u8>,
    },
    Shutdown,
}

struct Slot {
    kind_cpu: bool,
    seen: bool,
    disconnected_at: Option<Instant>,
}

struct Conn {
    login: Option<String>,
    token: Option<String>,
    tx: mpsc::Sender<String>,
}

pub struct RoomSettings {
    pub instant: bool,
    pub disconnect: Duration,
    pub result: Option<ResultCtx>,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            instant: false,
            disconnect: Duration::from_secs(DISCONNECT_SECS),
            result: None,
        }
    }
}

pub fn spawn_room(
    row: MatchRow,
    pool: SqlitePool,
    settings: RoomSettings,
) -> mpsc::Sender<RoomEvent> {
    let (tx, rx) = mpsc::channel(256);
    tokio::spawn(run_room(row, pool, settings, rx));
    tx
}

async fn run_room(
    row: MatchRow,
    pool: SqlitePool,
    settings: RoomSettings,
    mut rx: mpsc::Receiver<RoomEvent>,
) {
    let seed: u64 = row.seed.parse().unwrap_or(1);
    let delay = u32::try_from(row.input_delay_ticks).unwrap_or(INPUT_DELAY);
    let hunks = db::list_hunks(&pool, &row.id).await.unwrap_or_default();
    let github = db::github_identity(&row, &hunks);
    let total_rounds = u32::try_from(hunks.len()).unwrap_or(0).max(1);
    let mut round: u32 = hunks
        .iter()
        .find(|h| h.winner.is_none())
        .map(|h| h.round_index as u32)
        .unwrap_or(0)
        .min(total_rounds.saturating_sub(1));
    let (ours_stats, theirs_stats) = db::stats_for_round(&hunks, round);
    let mut sim = FightState::new(round_seed(seed, round), ours_stats, theirs_stats);
    let mut next_tick = 0u32;
    let mut log: Vec<(u32, u8, u8)> = Vec::new();
    if let Ok(inputs) = db::load_inputs(&pool, &row.id, round).await {
        for (tick, ours, theirs) in &inputs {
            if *tick == next_tick && sim.result.is_none() {
                sim.step(Input::from_u8(*ours), Input::from_u8(*theirs));
                log.push((*tick, *ours, *theirs));
                next_tick = next_tick.saturating_add(1);
            }
        }
    }

    let mut ours = Slot {
        kind_cpu: false,
        seen: false,
        disconnected_at: None,
    };
    let mut theirs = Slot {
        kind_cpu: false,
        seen: false,
        disconnected_at: None,
    };
    let mut conns: BTreeMap<u64, Conn> = BTreeMap::new();
    let mut pending_ours: BTreeMap<u32, u8> = BTreeMap::new();
    let mut pending_theirs: BTreeMap<u32, u8> = BTreeMap::new();
    let mut started_at: Option<Instant> = None;
    let mut done = sim.result.is_some() || row.status == "finished" || row.status == "expired";
    let mut mirror = false;
    let id = row.id.clone();
    let ours_name = row.ours_name.clone();
    let mut theirs_name = db::theirs_name_for_round(&hunks, round, &row.theirs_name);
    let expires_at = parse_rfc3339(&row.expires_at);
    apply_round_identity(
        &row,
        &hunks,
        round,
        github,
        &mut ours,
        &mut theirs,
        &mut theirs_name,
        &mut mirror,
        true,
    );

    let mut clock = tokio::time::interval(Duration::from_millis(1000 / 30));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    RoomEvent::Shutdown => {
                        if !done {
                            expire_now(&pool, &id, &conns, settings.result.as_ref()).await;
                        }
                        break;
                    }
                    RoomEvent::Join { conn_id, login, token, tx } => {
                        let conn = Conn { login, token, tx: tx.clone() };
                        let role = conn_role(&conn, &row, &hunks, round, github);
                        let confirmed = if next_tick == 0 { -1 } else { next_tick as i32 - 1 };
                        let hello = hello_msg(
                            &id,
                            round_seed(seed, round),
                            delay,
                            role.as_str(),
                            &ours_name,
                            &theirs_name,
                            round,
                            total_rounds,
                            confirmed,
                            db::stats_for_round(&hunks, round),
                        );
                        let _ = tx.send(encode(&hello)).await;
                        let snap = snapshot_msg(
                            round_seed(seed, round),
                            round,
                            confirmed,
                            db::stats_for_round(&hunks, round),
                            &log,
                        );
                        let _ = tx.send(encode(&snap)).await;
                        for &(n, o, t) in &log {
                            let _ = tx
                                .send(encode(&ServerMsg::Tick {
                                    n,
                                    ours: o,
                                    theirs: t,
                                }))
                                .await;
                        }
                        if let Some(result) = sim.result {
                            let match_over = round + 1 >= total_rounds;
                            let _ = tx.send(encode(&end_msg(&sim, result, round, match_over))).await;
                        }
                        conns.insert(conn_id, conn);
                        refresh_slots(
                            &conns,
                            &row,
                            &hunks,
                            round,
                            github,
                            &mut ours,
                            &mut theirs,
                            started_at.is_some(),
                        );
                        if !done && ours.seen && theirs.seen && started_at.is_none() {
                            started_at = Some(Instant::now());
                            let _ = db::set_status(
                                &pool, &id, "in_progress", true, false, None, None,
                            )
                            .await;
                        }
                    }
                    RoomEvent::Leave { conn_id } => {
                        conns.remove(&conn_id);
                        if !done {
                            refresh_slots(
                                &conns,
                                &row,
                                &hunks,
                                round,
                                github,
                                &mut ours,
                                &mut theirs,
                                started_at.is_some(),
                            );
                        }
                    }
                    RoomEvent::Input {
                        conn_id,
                        tick,
                        buttons,
                        theirs_buttons,
                    } => {
                        if done || sim.result.is_some() {
                            continue;
                        }
                        if tick < next_tick || tick > next_tick.saturating_add(INPUT_WINDOW) {
                            continue;
                        }
                        let Some(conn) = conns.get(&conn_id) else {
                            continue;
                        };
                        match conn_role(conn, &row, &hunks, round, github) {
                            Role::Ours => {
                                pending_ours.entry(tick).or_insert(buttons);
                            }
                            Role::Theirs => {
                                pending_theirs.entry(tick).or_insert(buttons);
                            }
                            Role::Both => {
                                pending_ours.entry(tick).or_insert(buttons);
                                pending_theirs
                                    .entry(tick)
                                    .or_insert(theirs_buttons.unwrap_or(0));
                            }
                            Role::Spectator => {}
                        }
                    }
                }
            }
            _ = clock.tick() => {
                if !done && started_at.is_none() {
                    if let Some(exp) = expires_at {
                        if Utc::now() >= exp {
                            expire_now(&pool, &id, &conns, settings.result.as_ref()).await;
                            done = true;
                        }
                    }
                }
            }
        }

        if done {
            continue;
        }

        done = advance(Advance {
            sim: &mut sim,
            next_tick: &mut next_tick,
            log: &mut log,
            pending_ours: &mut pending_ours,
            pending_theirs: &mut pending_theirs,
            ours: &mut ours,
            theirs: &mut theirs,
            conns: &conns,
            pool: &pool,
            id: &id,
            started_at: &mut started_at,
            instant: settings.instant,
            disconnect: settings.disconnect,
            round: &mut round,
            total_rounds,
            seed,
            delay,
            ours_name: &ours_name,
            theirs_name: &mut theirs_name,
            hunks: &hunks,
            match_id: &id,
            result: settings.result.clone(),
            mirror: &mut mirror,
            row: &row,
            github,
        })
        .await;
    }
}

struct Advance<'a> {
    sim: &'a mut FightState,
    next_tick: &'a mut u32,
    log: &'a mut Vec<(u32, u8, u8)>,
    pending_ours: &'a mut BTreeMap<u32, u8>,
    pending_theirs: &'a mut BTreeMap<u32, u8>,
    ours: &'a mut Slot,
    theirs: &'a mut Slot,
    conns: &'a BTreeMap<u64, Conn>,
    pool: &'a SqlitePool,
    id: &'a str,
    started_at: &'a mut Option<Instant>,
    instant: bool,
    disconnect: Duration,
    round: &'a mut u32,
    total_rounds: u32,
    seed: u64,
    delay: u32,
    ours_name: &'a str,
    theirs_name: &'a mut String,
    hunks: &'a [db::HunkRow],
    match_id: &'a str,
    result: Option<ResultCtx>,
    mirror: &'a mut bool,
    row: &'a MatchRow,
    github: bool,
}

async fn advance(a: Advance<'_>) -> bool {
    if let Some(result) = a.sim.result {
        return finish(a, result, false).await;
    }

    if let Some(at) = a.ours.disconnected_at {
        if at.elapsed() >= a.disconnect {
            a.sim.forfeit(Side::Ours);
            return finish(a, RoundResult::Theirs, true).await;
        }
    }
    if let Some(at) = a.theirs.disconnected_at {
        if at.elapsed() >= a.disconnect {
            a.sim.forfeit(Side::Theirs);
            return finish(a, RoundResult::Ours, true).await;
        }
    }

    if !a.ours.seen || !a.theirs.seen {
        return false;
    }
    if a.ours.disconnected_at.is_some() || a.theirs.disconnected_at.is_some() {
        return false;
    }

    let cap = if a.instant { 32 } else { 1 };
    for _ in 0..cap {
        if a.sim.result.is_some() {
            break;
        }
        if !a.instant {
            if let Some(start) = *a.started_at {
                let due = start + Duration::from_millis(u64::from(*a.next_tick) * 1000 / 30);
                if Instant::now() < due {
                    break;
                }
            }
        }
        let have_o = a.pending_ours.contains_key(a.next_tick) || a.ours.kind_cpu;
        let have_t = a.pending_theirs.contains_key(a.next_tick) || a.theirs.kind_cpu;
        if !have_o || !have_t {
            if a.instant {
                break;
            }
            if let Some(start) = *a.started_at {
                let wait_until =
                    start + Duration::from_millis(u64::from(*a.next_tick) * 1000 / 30 + 66);
                if Instant::now() < wait_until {
                    break;
                }
            } else {
                break;
            }
        }
        let ours_btn = if a.ours.kind_cpu {
            a.sim.cpu_input(Side::Ours).as_u8()
        } else {
            a.pending_ours.remove(a.next_tick).unwrap_or(0)
        };
        let theirs_btn = if a.theirs.kind_cpu {
            a.sim.cpu_input(Side::Theirs).as_u8()
        } else {
            a.pending_theirs.remove(a.next_tick).unwrap_or(0)
        };
        a.sim
            .step(Input::from_u8(ours_btn), Input::from_u8(theirs_btn));
        let n = *a.next_tick;
        *a.next_tick = a.next_tick.saturating_add(1);
        a.log.push((n, ours_btn, theirs_btn));
        let _ = db::insert_input(a.pool, a.id, *a.round, n, ours_btn, theirs_btn).await;
        let msg = encode(&ServerMsg::Tick {
            n,
            ours: ours_btn,
            theirs: theirs_btn,
        });
        broadcast(a.conns, &msg).await;
        if n % 10 == 9 || a.sim.result.is_some() {
            let (lo, hi) = split_hash(a.sim.state_hash());
            let hash = encode(&ServerMsg::Hash {
                n: a.sim.tick,
                hi,
                lo,
            });
            broadcast(a.conns, &hash).await;
            let confirmed = if *a.next_tick == 0 {
                -1
            } else {
                *a.next_tick as i32 - 1
            };
            let snap = snapshot_msg(
                round_seed(a.seed, *a.round),
                *a.round,
                confirmed,
                db::stats_for_round(a.hunks, *a.round),
                a.log,
            );
            broadcast(a.conns, &encode(&snap)).await;
        }
    }
    if let Some(result) = a.sim.result {
        return finish(a, result, false).await;
    }
    false
}

async fn finish(a: Advance<'_>, result: RoundResult, forfeit: bool) -> bool {
    let tag = result::winner_tag(result, forfeit);
    let ko =
        !forfeit && result != RoundResult::Draw && (a.sim.ours.hp <= 0 || a.sim.theirs.hp <= 0);
    let _ = db::set_hunk_winner(a.pool, a.id, i64::from(*a.round), tag).await;
    let _ = crate::stats::record_round(a.pool, a.id, i64::from(*a.round), tag, ko).await;
    let match_over = *a.round + 1 >= a.total_rounds;
    let (lo, hi) = split_hash(a.sim.state_hash());
    let hash_s = format!("{hi:08x}{lo:08x}");
    if match_over {
        let _ = db::set_status(a.pool, a.id, "finished", true, true, Some(&hash_s), None).await;
        if let Some(ctx) = a.result {
            let id = a.id.to_string();
            tokio::spawn(async move {
                if let Err(e) = result::publish(&ctx, &id).await {
                    eprintln!("git fight result: {e}");
                }
            });
        }
    }
    let msg = encode(&end_msg(a.sim, result, *a.round, match_over));
    broadcast(a.conns, &msg).await;
    if match_over {
        return true;
    }
    *a.round += 1;
    let (ours_stats, theirs_stats) = db::stats_for_round(a.hunks, *a.round);
    *a.sim = FightState::new(round_seed(a.seed, *a.round), ours_stats, theirs_stats);
    *a.next_tick = 0;
    a.log.clear();
    a.pending_ours.clear();
    a.pending_theirs.clear();
    apply_round_identity(
        a.row,
        a.hunks,
        *a.round,
        a.github,
        a.ours,
        a.theirs,
        a.theirs_name,
        a.mirror,
        false,
    );
    refresh_slots(
        a.conns, a.row, a.hunks, *a.round, a.github, a.ours, a.theirs, true,
    );
    *a.started_at = Some(Instant::now());
    for conn in a.conns.values() {
        let role = conn_role(conn, a.row, a.hunks, *a.round, a.github);
        let hello = hello_msg(
            a.match_id,
            round_seed(a.seed, *a.round),
            a.delay,
            role.as_str(),
            a.ours_name,
            a.theirs_name,
            *a.round,
            a.total_rounds,
            -1,
            db::stats_for_round(a.hunks, *a.round),
        );
        let _ = conn.tx.send(encode(&hello)).await;
    }
    false
}

async fn expire_now(
    pool: &SqlitePool,
    id: &str,
    conns: &BTreeMap<u64, Conn>,
    result: Option<&ResultCtx>,
) {
    let row = db::get_match(pool, id).await.ok().flatten();
    if row.as_ref().is_some_and(|r| r.status == "expired") {
        let msg = encode(&ServerMsg::Error {
            message: "expired".into(),
        });
        broadcast(conns, &msg).await;
        return;
    }
    let _ = db::set_status(pool, id, "expired", false, true, None, Some("expired")).await;
    if let (Some(ctx), Some(row)) = (result, row) {
        result::comment_expired(ctx, &row).await;
    }
    let msg = encode(&ServerMsg::Error {
        message: "expired".into(),
    });
    broadcast(conns, &msg).await;
}

fn end_msg(sim: &FightState, result: RoundResult, round: u32, match_over: bool) -> ServerMsg {
    let (lo, hi) = split_hash(sim.state_hash());
    let result_i = match result {
        RoundResult::Ours => 0,
        RoundResult::Theirs => 1,
        RoundResult::Draw => 2,
    };
    ServerMsg::End {
        result: result_i,
        hash_hi: hi,
        hash_lo: lo,
        tick: sim.tick,
        round,
        match_over,
    }
}

#[allow(clippy::too_many_arguments)]
fn hello_msg(
    match_id: &str,
    seed: u64,
    delay: u32,
    role: &str,
    ours: &str,
    theirs: &str,
    round: u32,
    total_rounds: u32,
    confirmed_tick: i32,
    stats: (FighterStats, FighterStats),
) -> ServerMsg {
    let (seed_lo, seed_hi) = split_seed(seed);
    let (ours_stats, theirs_stats) = stats;
    ServerMsg::Hello {
        match_id: match_id.to_string(),
        seed_lo,
        seed_hi,
        input_delay: delay,
        your_role: role.to_string(),
        ours: ours.to_string(),
        theirs: theirs.to_string(),
        round,
        total_rounds,
        confirmed_tick,
        ours_hp: ours_stats.hp,
        ours_armor: ours_stats.armor,
        ours_special: ours_stats.special,
        theirs_hp: theirs_stats.hp,
        theirs_armor: theirs_stats.armor,
        theirs_special: theirs_stats.special,
    }
}

fn snapshot_msg(
    seed: u64,
    round: u32,
    confirmed_tick: i32,
    stats: (FighterStats, FighterStats),
    log: &[(u32, u8, u8)],
) -> ServerMsg {
    let (seed_lo, seed_hi) = split_seed(seed);
    let (ours_stats, theirs_stats) = stats;
    ServerMsg::Snapshot {
        seed_lo,
        seed_hi,
        round,
        confirmed_tick,
        ours_hp: ours_stats.hp,
        ours_armor: ours_stats.armor,
        ours_special: ours_stats.special,
        theirs_hp: theirs_stats.hp,
        theirs_armor: theirs_stats.armor,
        theirs_special: theirs_stats.special,
        ticks: log.to_vec(),
    }
}

async fn broadcast(conns: &BTreeMap<u64, Conn>, msg: &str) {
    for conn in conns.values() {
        let _ = conn.tx.send(msg.to_string()).await;
    }
}

fn encode(msg: &ServerMsg) -> String {
    serde_json::to_string(msg).unwrap_or_else(|_| r#"{"type":"error","message":"encode"}"#.into())
}

fn parse_rfc3339(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

fn conn_role(conn: &Conn, row: &MatchRow, hunks: &[db::HunkRow], round: u32, github: bool) -> Role {
    role_for(
        github,
        row.ours_login.as_deref(),
        db::theirs_login_for_round(hunks, round, row.theirs_login.as_deref()),
        conn.login.as_deref(),
        conn.token.as_deref(),
        row.ours_token.as_deref(),
        row.theirs_token.as_deref(),
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_round_identity(
    row: &MatchRow,
    hunks: &[db::HunkRow],
    round: u32,
    github: bool,
    ours: &mut Slot,
    theirs: &mut Slot,
    theirs_name: &mut String,
    mirror: &mut bool,
    initial: bool,
) {
    *theirs_name = db::theirs_name_for_round(hunks, round, &row.theirs_name);
    if github {
        let ours_login = row.ours_login.as_deref();
        let t_login = db::theirs_login_for_round(hunks, round, row.theirs_login.as_deref());
        *mirror = t_login.is_some() && t_login == ours_login;
        ours.kind_cpu = row.ours_kind == "cpu";
        theirs.kind_cpu = !*mirror
            && (t_login.is_none()
                || (row.theirs_kind == "cpu" && t_login == row.theirs_login.as_deref()));
    } else {
        *mirror = row.ours_kind == "mirror" || row.theirs_kind == "mirror";
        ours.kind_cpu = row.ours_kind == "cpu";
        theirs.kind_cpu = row.theirs_kind == "cpu";
    }
    if ours.kind_cpu {
        ours.seen = true;
        ours.disconnected_at = None;
    } else if !initial {
        ours.disconnected_at = None;
    }
    if theirs.kind_cpu {
        theirs.seen = true;
        theirs.disconnected_at = None;
    } else if !initial {
        theirs.seen = false;
        theirs.disconnected_at = None;
    }
}

#[allow(clippy::too_many_arguments)]
fn refresh_slots(
    conns: &BTreeMap<u64, Conn>,
    row: &MatchRow,
    hunks: &[db::HunkRow],
    round: u32,
    github: bool,
    ours: &mut Slot,
    theirs: &mut Slot,
    match_started: bool,
) {
    let mut ours_here = ours.kind_cpu;
    let mut theirs_here = theirs.kind_cpu;
    for conn in conns.values() {
        match conn_role(conn, row, hunks, round, github) {
            Role::Ours => ours_here = true,
            Role::Theirs => theirs_here = true,
            Role::Both => {
                ours_here = true;
                theirs_here = true;
            }
            Role::Spectator => {}
        }
    }
    apply_presence(ours, ours_here, match_started);
    apply_presence(theirs, theirs_here, match_started);
}

fn apply_presence(slot: &mut Slot, here: bool, match_started: bool) {
    if here {
        slot.seen = true;
        slot.disconnected_at = None;
        return;
    }
    if slot.kind_cpu {
        slot.disconnected_at = None;
        return;
    }
    if (slot.seen || match_started) && slot.disconnected_at.is_none() {
        slot.disconnected_at = Some(Instant::now());
    }
}
