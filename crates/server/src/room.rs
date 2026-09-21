use crate::db::{self, MatchRow};
use crate::protocol::{
    closed_ws_message, role_for, round_seed, split_hash, split_seed, Role, ServerMsg,
    DISCONNECT_SECS, INPUT_DELAY, INPUT_WINDOW,
};
use crate::result::{self, ResultCtx};
use chrono::{DateTime, Utc};
use git_fight_core::{FightState, FighterStats, Input, RoundResult, Side};
use sqlx::SqlitePool;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};
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
        round: Option<u32>,
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
    rooms: Arc<Mutex<HashMap<String, mpsc::Sender<RoomEvent>>>>,
) -> mpsc::Sender<RoomEvent> {
    let (tx, rx) = mpsc::channel(512);
    let id = row.id.clone();
    let posted = tx.clone();
    tokio::spawn(async move {
        run_room(row, pool, settings, rx).await;
        let mut rooms = rooms.lock().await;
        if rooms.get(&id).is_some_and(|t| t.same_channel(&posted)) {
            rooms.remove(&id);
        }
    });
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
    let scored_all = !hunks.is_empty() && hunks.iter().all(|h| h.winner.is_some());
    let mut round: u32 = hunks
        .iter()
        .find(|h| h.winner.is_none())
        .and_then(|h| u32::try_from(h.round_index).ok())
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
    let mut forfeit_pending = false;
    let id = row.id.clone();
    let mut done = matches!(row.status.as_str(), "finished" | "expired" | "aborted");
    if !done && scored_all {
        crate::stats::record_stored_winners(&pool, &id, &hunks).await;
        let last = total_rounds.saturating_sub(1);
        let hash = hash_from_stored_round(&pool, &id, seed, &hunks, last).await;
        if db::finish_open_match(&pool, &id, &hash)
            .await
            .unwrap_or(false)
        {
            if let Some(ctx) = settings.result.as_ref() {
                ctx.spawn_publish(id.clone());
            }
        }
        done = true;
    }
    let mut mirror = false;
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

    let mut drain_until: Option<tokio::time::Instant> = None;
    loop {
        if done {
            let until = *drain_until
                .get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(2));
            tokio::select! {
                ev = rx.recv() => {
                    match ev {
                        None | Some(RoomEvent::Shutdown) => break,
                        Some(RoomEvent::Join { tx, .. }) => {
                            send_closed(&tx, &pool, &id).await;
                        }
                        Some(RoomEvent::Leave { conn_id }) => {
                            conns.remove(&conn_id);
                            if conns.is_empty() {
                                break;
                            }
                        }
                        Some(RoomEvent::Input { .. }) => {}
                    }
                }
                _ = tokio::time::sleep_until(until) => break,
            }
            continue;
        }
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
                        if done {
                            send_closed(&tx, &pool, &id).await;
                        } else if match_is_open(&pool, &id).await == MatchOpen::Closed {
                            send_closed(&tx, &pool, &id).await;
                            expire_now(&pool, &id, &conns, settings.result.as_ref()).await;
                            done = true;
                        } else {
                            let conn = Conn { login, token, tx: tx.clone() };
                            let role = conn_role(&conn, &row, &hunks, round, github);
                            let confirmed = if next_tick == 0 { -1 } else { next_tick as i32 - 1 };
                            conns.insert(conn_id, conn);
                            refresh_slots(
                                &conns,
                                &row,
                                &hunks,
                                round,
                                github,
                                &mut ours,
                                &mut theirs,
                            );
                            if ours.seen && theirs.seen && started_at.is_none() {
                                match db::start_open_match(&pool, &id).await {
                                    Ok(true) => started_at = Some(Instant::now()),
                                    Ok(false) => {
                                        send_closed(&tx, &pool, &id).await;
                                        expire_now(
                                            &pool,
                                            &id,
                                            &conns,
                                            settings.result.as_ref(),
                                        )
                                        .await;
                                        done = true;
                                    }
                                    Err(_) => {}
                                }
                            }
                            if !done {
                                let hello = hello_msg(
                                    &id,
                                    round_seed(seed, round),
                                    delay,
                                    role.as_str(),
                                    conns
                                        .get(&conn_id)
                                        .and_then(|c| c.login.as_deref())
                                        .unwrap_or(""),
                                    &ours_name,
                                    &theirs_name,
                                    round,
                                    total_rounds,
                                    confirmed,
                                    db::stats_for_round(&hunks, round),
                                    &hunks,
                                );
                                try_send_or_spawn(&tx, encode(&hello));
                                let snap = snapshot_msg(
                                    round_seed(seed, round),
                                    round,
                                    confirmed,
                                    db::stats_for_round(&hunks, round),
                                    &log,
                                    &hunks,
                                );
                                try_send_or_spawn(&tx, encode(&snap));
                                if let Some(result) = sim.result {
                                    let match_over = round + 1 >= total_rounds;
                                    try_send_or_spawn(
                                        &tx,
                                        encode(&end_msg(&sim, result, round, match_over)),
                                    );
                                }
                            }
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
                            );
                        }
                    }
                    RoomEvent::Input {
                        conn_id,
                        tick,
                        buttons,
                        theirs_buttons,
                        round: input_round,
                    } => {
                        if done || sim.result.is_some() {
                            continue;
                        }
                        // GitHub matches require the Hello round so a leftover
                        // KO Input that omitted `round` cannot steer the next
                        // conflict. Local demo still accepts omitted round.
                        if github {
                            if input_round != Some(round) {
                                continue;
                            }
                        } else if input_round.is_some_and(|r| r != round) {
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
                if !done {
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
            forfeit_pending: &mut forfeit_pending,
        })
        .await;
    }
    drain_late_joins(&mut rx, &pool, &id).await;
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
    forfeit_pending: &'a mut bool,
}

async fn advance(a: Advance<'_>) -> bool {
    match match_is_open(a.pool, a.id).await {
        MatchOpen::Closed => {
            expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
            return true;
        }
        MatchOpen::Unknown => return false,
        MatchOpen::Open => {}
    }
    if let Some(result) = a.sim.result {
        return finish(a, result).await;
    }
    if a.started_at.is_none() && a.ours.seen && a.theirs.seen {
        match db::start_open_match(a.pool, a.id).await {
            Ok(true) => *a.started_at = Some(Instant::now()),
            Ok(false) => {
                expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
                return true;
            }
            Err(_) => return false,
        }
    }

    // Disconnect forfeit is only for someone who already occupied a slot
    // after the match started. A fighter who has not shown up yet waits
    // until expires_at (24h), not 30 seconds.
    if a.started_at.is_some() {
        if let Some(at) = a.ours.disconnected_at {
            if at.elapsed() >= a.disconnect {
                a.sim.forfeit(Side::Ours);
                *a.forfeit_pending = true;
                return finish(a, RoundResult::Theirs).await;
            }
        }
        if let Some(at) = a.theirs.disconnected_at {
            if at.elapsed() >= a.disconnect {
                a.sim.forfeit(Side::Theirs);
                *a.forfeit_pending = true;
                return finish(a, RoundResult::Ours).await;
            }
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
        match match_is_open(a.pool, a.id).await {
            MatchOpen::Closed => {
                expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
                return true;
            }
            MatchOpen::Unknown => return false,
            MatchOpen::Open => {}
        }
        let ours_btn = if a.ours.kind_cpu {
            a.sim.cpu_input(Side::Ours).as_u8()
        } else {
            a.pending_ours.get(a.next_tick).copied().unwrap_or(0)
        };
        let theirs_btn = if a.theirs.kind_cpu {
            a.sim.cpu_input(Side::Theirs).as_u8()
        } else {
            a.pending_theirs.get(a.next_tick).copied().unwrap_or(0)
        };
        match db::persist_input(a.pool, a.id, *a.round, *a.next_tick, ours_btn, theirs_btn).await {
            Ok(true) => {}
            Ok(false) => {
                if match_is_open(a.pool, a.id).await == MatchOpen::Closed {
                    expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
                    return true;
                }
                return false;
            }
            Err(_) => return false,
        }
        if !a.ours.kind_cpu {
            a.pending_ours.remove(a.next_tick);
        }
        if !a.theirs.kind_cpu {
            a.pending_theirs.remove(a.next_tick);
        }
        a.sim
            .step(Input::from_u8(ours_btn), Input::from_u8(theirs_btn));
        let n = *a.next_tick;
        *a.next_tick = a.next_tick.saturating_add(1);
        a.log.push((n, ours_btn, theirs_btn));
        let msg = encode(&ServerMsg::Tick {
            n,
            ours: ours_btn,
            theirs: theirs_btn,
        });
        broadcast(a.conns, &msg);
        if n % 10 == 9 || a.sim.result.is_some() {
            let (lo, hi) = split_hash(a.sim.state_hash());
            let hash = encode(&ServerMsg::Hash {
                n: a.sim.tick,
                hi,
                lo,
            });
            broadcast(a.conns, &hash);
        }
    }
    if let Some(result) = a.sim.result {
        return finish(a, result).await;
    }
    false
}

/// What to do after `set_hunk_winner`. A failed write with no stored
/// winner must retry — do not broadcast End or start the next conflict.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FinishAfterWrite {
    Proceed { record: bool },
    Close,
    Retry,
}

fn finish_after_write(tagged: bool, open: MatchOpen, stored: StoredRound) -> FinishAfterWrite {
    if tagged {
        FinishAfterWrite::Proceed { record: true }
    } else if open == MatchOpen::Closed {
        FinishAfterWrite::Close
    } else if matches!(stored, StoredRound::Winner | StoredRound::Missing) {
        // Winner: write-once resume. Missing: local demo / no hunk row —
        // there is nothing to score; still End. Do not retry forever.
        FinishAfterWrite::Proceed { record: false }
    } else {
        FinishAfterWrite::Retry
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MatchOpen {
    Open,
    Closed,
    Unknown,
}

async fn match_is_open(pool: &SqlitePool, id: &str) -> MatchOpen {
    match db::is_open_match(pool, id).await {
        Ok(true) => MatchOpen::Open,
        Ok(false) => MatchOpen::Closed,
        Err(_) => MatchOpen::Unknown,
    }
}

/// Last round is done only after `finish_open_match` or a known close.
/// A busy mark must retry so a won fight cannot sit `in_progress` until expiry.
fn last_round_done(marked: bool, open: MatchOpen) -> bool {
    marked || open == MatchOpen::Closed
}

/// After a non-final winner is stored, broadcast End only when the row is
/// known still open. Unknown retries without End.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AfterNonFinal {
    EndAndAdvance,
    Expire,
    Retry,
}

fn after_non_final(open: MatchOpen) -> AfterNonFinal {
    match open {
        MatchOpen::Open => AfterNonFinal::EndAndAdvance,
        MatchOpen::Closed => AfterNonFinal::Expire,
        MatchOpen::Unknown => AfterNonFinal::Retry,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StoredRound {
    Winner,
    Empty,
    Missing,
    Unknown,
}

async fn stored_round(pool: &SqlitePool, id: &str, round: u32) -> StoredRound {
    match db::list_hunks(pool, id).await {
        Err(_) => StoredRound::Unknown,
        Ok(hs) => match hs.into_iter().find(|h| h.round_index == i64::from(round)) {
            None => StoredRound::Missing,
            Some(h) if h.winner.is_some() => StoredRound::Winner,
            Some(_) => StoredRound::Empty,
        },
    }
}

async fn finish(a: Advance<'_>, result: RoundResult) -> bool {
    match match_is_open(a.pool, a.id).await {
        MatchOpen::Closed => {
            expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
            return true;
        }
        MatchOpen::Unknown => return false,
        MatchOpen::Open => {}
    }
    let forfeit = *a.forfeit_pending;
    let tag = result::winner_tag(result, forfeit);
    let ko =
        !forfeit && result != RoundResult::Draw && (a.sim.ours.hp <= 0 || a.sim.theirs.hp <= 0);
    let tagged = db::set_hunk_winner(a.pool, a.id, i64::from(*a.round), tag, ko)
        .await
        .unwrap_or(false);
    let open = match_is_open(a.pool, a.id).await;
    let stored = if tagged {
        StoredRound::Winner
    } else {
        stored_round(a.pool, a.id, *a.round).await
    };
    match finish_after_write(tagged, open, stored) {
        FinishAfterWrite::Proceed { record: true } => {
            *a.forfeit_pending = false;
            let _ = crate::stats::record_round(a.pool, a.id, i64::from(*a.round), tag, ko).await;
        }
        FinishAfterWrite::Proceed { record: false } => {
            *a.forfeit_pending = false;
            let _ = crate::stats::record_round(a.pool, a.id, i64::from(*a.round), tag, ko).await;
        }
        FinishAfterWrite::Close => {
            *a.forfeit_pending = false;
            expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
            return true;
        }
        FinishAfterWrite::Retry => return false,
    }
    let match_over = *a.round + 1 >= a.total_rounds;
    let (lo, hi) = split_hash(a.sim.state_hash());
    let hash_s = format!("{hi:08x}{lo:08x}");
    if match_over {
        let marked = db::finish_open_match(a.pool, a.id, &hash_s)
            .await
            .unwrap_or(false);
        if marked {
            if let Some(ctx) = a.result.as_ref() {
                ctx.spawn_publish(a.id.to_string());
            }
        } else if !last_round_done(false, match_is_open(a.pool, a.id).await) {
            return false;
        }
        let msg = encode(&end_msg(a.sim, result, *a.round, true));
        broadcast(a.conns, &msg);
        return true;
    }
    // Clients advance Input.round on End. A busy open-status read must
    // retry without that broadcast so GitHub inputs for the next conflict
    // are not dropped while this room is still on the finished round.
    match after_non_final(match_is_open(a.pool, a.id).await) {
        AfterNonFinal::Expire => {
            expire_now(a.pool, a.id, a.conns, a.result.as_ref()).await;
            return true;
        }
        AfterNonFinal::Retry => return false,
        AfterNonFinal::EndAndAdvance => {}
    }
    let msg = encode(&end_msg(a.sim, result, *a.round, false));
    broadcast(a.conns, &msg);
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
        a.conns, a.row, a.hunks, *a.round, a.github, a.ours, a.theirs,
    );
    *a.started_at = Some(Instant::now());
    for conn in a.conns.values() {
        let role = conn_role(conn, a.row, a.hunks, *a.round, a.github);
        let hello = hello_msg(
            a.match_id,
            round_seed(a.seed, *a.round),
            a.delay,
            role.as_str(),
            conn.login.as_deref().unwrap_or(""),
            a.ours_name,
            a.theirs_name,
            *a.round,
            a.total_rounds,
            -1,
            db::stats_for_round(a.hunks, *a.round),
            a.hunks,
        );
        try_send_or_spawn(&conn.tx, encode(&hello));
    }
    false
}

pub(crate) async fn hash_from_stored_round(
    pool: &SqlitePool,
    id: &str,
    seed: u64,
    hunks: &[db::HunkRow],
    round: u32,
) -> String {
    let (ours_stats, theirs_stats) = db::stats_for_round(hunks, round);
    let mut sim = FightState::new(round_seed(seed, round), ours_stats, theirs_stats);
    if let Ok(inputs) = db::load_inputs(pool, id, round).await {
        for (_tick, ours, theirs) in inputs {
            if sim.result.is_some() {
                break;
            }
            sim.step(Input::from_u8(ours), Input::from_u8(theirs));
        }
    }
    let (lo, hi) = split_hash(sim.state_hash());
    format!("{hi:08x}{lo:08x}")
}

fn terminal_ws_error(row: &MatchRow) -> String {
    closed_ws_message(&row.status, row.abort_reason.as_deref())
        .unwrap_or(row.status.as_str())
        .to_string()
}

async fn closed_message(pool: &SqlitePool, id: &str) -> String {
    match db::get_match(pool, id).await {
        Ok(Some(row)) => terminal_ws_error(&row),
        _ => "finished".into(),
    }
}

async fn send_closed(tx: &mpsc::Sender<String>, pool: &SqlitePool, id: &str) {
    let message = closed_message(pool, id).await;
    try_send_or_spawn(tx, encode(&ServerMsg::Error { message }));
}

async fn drain_late_joins(rx: &mut mpsc::Receiver<RoomEvent>, pool: &SqlitePool, id: &str) {
    let message = closed_message(pool, id).await;
    while let Ok(ev) = rx.try_recv() {
        if let RoomEvent::Join { tx, .. } = ev {
            try_send_or_spawn(
                &tx,
                encode(&ServerMsg::Error {
                    message: message.clone(),
                }),
            );
        }
    }
}

async fn expire_now(
    pool: &SqlitePool,
    id: &str,
    conns: &BTreeMap<u64, Conn>,
    result: Option<&ResultCtx>,
) {
    let row = db::get_match(pool, id).await.ok().flatten();
    if let Some(row) = row.as_ref() {
        if matches!(row.status.as_str(), "expired" | "aborted" | "finished") {
            let msg = encode(&ServerMsg::Error {
                message: terminal_ws_error(row),
            });
            broadcast(conns, &msg);
            return;
        }
    }
    if !db::expire_open_match(pool, id).await.unwrap_or(false) {
        let row = db::get_match(pool, id).await.ok().flatten();
        if let Some(row) = row.as_ref() {
            let msg = encode(&ServerMsg::Error {
                message: terminal_ws_error(row),
            });
            broadcast(conns, &msg);
        }
        return;
    }
    if let (Some(ctx), Some(row)) = (result, row.as_ref()) {
        let ctx = ctx.clone();
        let row = row.clone();
        tokio::spawn(async move {
            result::comment_expired(&ctx, &row).await;
        });
    }
    let msg = encode(&ServerMsg::Error {
        message: "expired".into(),
    });
    broadcast(conns, &msg);
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
    you_are: &str,
    ours: &str,
    theirs: &str,
    round: u32,
    total_rounds: u32,
    confirmed_tick: i32,
    stats: (FighterStats, FighterStats),
    hunks: &[db::HunkRow],
) -> ServerMsg {
    let (seed_lo, seed_hi) = split_seed(seed);
    let (ours_stats, theirs_stats) = stats;
    let (path, hunk_index) = db::hunk_meta_for_round(hunks, round);
    ServerMsg::Hello {
        match_id: match_id.to_string(),
        seed_lo,
        seed_hi,
        input_delay: delay,
        your_role: role.to_string(),
        you_are: you_are.to_string(),
        ours: ours.to_string(),
        theirs: theirs.to_string(),
        round,
        total_rounds,
        confirmed_tick,
        path,
        hunk_index,
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
    hunks: &[db::HunkRow],
) -> ServerMsg {
    let (seed_lo, seed_hi) = split_seed(seed);
    let (ours_stats, theirs_stats) = stats;
    let (path, hunk_index) = db::hunk_meta_for_round(hunks, round);
    ServerMsg::Snapshot {
        seed_lo,
        seed_hi,
        round,
        confirmed_tick,
        path,
        hunk_index,
        ours_hp: ours_stats.hp,
        ours_armor: ours_stats.armor,
        ours_special: ours_stats.special,
        theirs_hp: theirs_stats.hp,
        theirs_armor: theirs_stats.armor,
        theirs_special: theirs_stats.special,
        ticks: log.to_vec(),
    }
}

/// Non-blocking: a spectator (or fighter) who stops reading must not stall
/// the 30 Hz confirm loop. Missed ticks are recovered via Snapshot on reconnect.
fn broadcast(conns: &BTreeMap<u64, Conn>, msg: &str) {
    for conn in conns.values() {
        let _ = conn.tx.try_send(msg.to_string());
    }
}

/// Join Hello/Snapshot and next-round Hello must not vanish when the
/// outbound channel is full. Ticks can be skipped (Snapshot on reconnect);
/// a dropped Hello leaves `--instant` waiting forever for Input tagged
/// with the new round.
fn try_send_or_spawn(tx: &mpsc::Sender<String>, msg: String) {
    if let Err(err) = tx.try_send(msg) {
        match err {
            mpsc::error::TrySendError::Full(msg) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    let _ = tx.send(msg).await;
                });
            }
            mpsc::error::TrySendError::Closed(_) => {}
        }
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
        *mirror = crate::gh::same_github_login(t_login, ours_login);
        ours.kind_cpu = row.ours_kind == "cpu";
        theirs.kind_cpu = !*mirror
            && (t_login.is_none()
                || (row.theirs_kind == "cpu"
                    && crate::gh::same_github_login(t_login, row.theirs_login.as_deref())));
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
        theirs.disconnected_at = None;
        // A new blamed author has not occupied this slot. Keep `seen`
        // false so apply_presence does not start a disconnect clock.
        if github && round > 0 {
            let prev = db::theirs_login_for_round(hunks, round - 1, row.theirs_login.as_deref());
            let new = db::theirs_login_for_round(hunks, round, row.theirs_login.as_deref());
            if !crate::gh::same_github_login(prev, new) {
                theirs.seen = false;
            }
        }
    }
}

fn refresh_slots(
    conns: &BTreeMap<u64, Conn>,
    row: &MatchRow,
    hunks: &[db::HunkRow],
    round: u32,
    github: bool,
    ours: &mut Slot,
    theirs: &mut Slot,
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
    apply_presence(ours, ours_here);
    apply_presence(theirs, theirs_here);
}

/// Start the 30s rejoin clock only after this login has occupied the slot.
/// `match_started` must not count: a later-round blamed author who has never
/// been here is "never showed up" (wait until expires_at), not a disconnect.
fn apply_presence(slot: &mut Slot, here: bool) {
    if here {
        slot.seen = true;
        slot.disconnected_at = None;
        return;
    }
    if slot.kind_cpu {
        slot.disconnected_at = None;
        return;
    }
    if slot.seen && slot.disconnected_at.is_none() {
        slot.disconnected_at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn full_client_buffer_does_not_block_broadcast() {
        let (tx, _rx) = mpsc::channel::<String>(1);
        tx.try_send("held".into()).unwrap();
        let mut conns = BTreeMap::new();
        conns.insert(
            1,
            Conn {
                login: None,
                token: None,
                tx,
            },
        );
        let hung = tokio::time::timeout(Duration::from_millis(200), async {
            broadcast(&conns, r#"{"type":"tick","n":0,"ours":0,"theirs":0}"#);
        })
        .await;
        assert!(hung.is_ok(), "broadcast waited on a full spectator buffer");
    }

    #[test]
    fn unseen_slot_does_not_start_disconnect_clock() {
        let mut slot = Slot {
            kind_cpu: false,
            seen: false,
            disconnected_at: None,
        };
        apply_presence(&mut slot, false);
        assert!(
            slot.disconnected_at.is_none(),
            "a login that has not occupied the slot is not on the 30s clock"
        );
        assert!(!slot.seen);
    }

    #[test]
    fn seen_fighter_who_left_starts_disconnect_clock() {
        let mut slot = Slot {
            kind_cpu: false,
            seen: true,
            disconnected_at: None,
        };
        apply_presence(&mut slot, false);
        assert!(slot.disconnected_at.is_some());
        assert!(slot.seen);
    }

    #[test]
    fn later_round_author_clears_seen() {
        let row = MatchRow {
            id: "m".into(),
            seed: "1".into(),
            status: "in_progress".into(),
            ours_name: "alice".into(),
            theirs_name: "bob".into(),
            ours_kind: "github".into(),
            theirs_kind: "github".into(),
            ours_token: None,
            theirs_token: None,
            ours_login: Some("alice".into()),
            theirs_login: Some("bob".into()),
            owner: String::new(),
            repo: String::new(),
            pr_number: 0,
            pr_head_sha: String::new(),
            pr_base_sha: String::new(),
            installation_id: None,
            input_delay_ticks: 3,
            created_at: String::new(),
            expires_at: String::new(),
            result_branch: None,
            final_hash: None,
            abort_reason: None,
            challenge_comment_id: None,
        };
        let hunks = vec![
            db::HunkRow {
                round_index: 0,
                path: "a.rs".into(),
                hunk_index: 0,
                winner: None,
                theirs_name: Some("bob".into()),
                theirs_login: Some("bob".into()),
                ours_hp: 100,
                ours_armor: false,
                ours_special: false,
                theirs_hp: 100,
                theirs_armor: false,
                theirs_special: false,
                is_ko: false,
            },
            db::HunkRow {
                round_index: 1,
                path: "b.rs".into(),
                hunk_index: 0,
                winner: None,
                theirs_name: Some("carol".into()),
                theirs_login: Some("carol".into()),
                ours_hp: 100,
                ours_armor: false,
                ours_special: false,
                theirs_hp: 100,
                theirs_armor: false,
                theirs_special: false,
                is_ko: false,
            },
        ];
        let mut ours = Slot {
            kind_cpu: false,
            seen: true,
            disconnected_at: None,
        };
        let mut theirs = Slot {
            kind_cpu: false,
            seen: true,
            disconnected_at: Some(Instant::now()),
        };
        let mut theirs_name = String::from("bob");
        let mut mirror = false;
        apply_round_identity(
            &row,
            &hunks,
            1,
            true,
            &mut ours,
            &mut theirs,
            &mut theirs_name,
            &mut mirror,
            false,
        );
        assert!(!theirs.seen, "carol has not occupied the right slot");
        assert!(theirs.disconnected_at.is_none());
        assert_eq!(theirs_name, "carol");
        apply_presence(&mut theirs, false);
        assert!(
            theirs.disconnected_at.is_none(),
            "carol must not be forfeited before she joins"
        );
    }

    #[test]
    fn finish_retries_when_winner_write_fails_and_nothing_is_stored() {
        assert_eq!(
            finish_after_write(true, MatchOpen::Open, StoredRound::Empty),
            FinishAfterWrite::Proceed { record: true }
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Closed, StoredRound::Empty),
            FinishAfterWrite::Close
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Open, StoredRound::Winner),
            FinishAfterWrite::Proceed { record: false },
            "write-once already set: resume End / next round"
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Open, StoredRound::Empty),
            FinishAfterWrite::Retry,
            "SQLite miss must not advance as if the hunk was scored"
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Unknown, StoredRound::Empty),
            FinishAfterWrite::Retry,
            "a busy status read is not a closed match"
        );
        assert_eq!(
            finish_after_write(true, MatchOpen::Unknown, StoredRound::Empty),
            FinishAfterWrite::Proceed { record: true },
            "a successful write still records even if the follow-up status read fails"
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Unknown, StoredRound::Winner),
            FinishAfterWrite::Proceed { record: false },
            "write-once already set: do not expire on a busy status read"
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Open, StoredRound::Missing),
            FinishAfterWrite::Proceed { record: false },
            "local demo has no hunk row; still End"
        );
        assert_eq!(
            finish_after_write(false, MatchOpen::Open, StoredRound::Unknown),
            FinishAfterWrite::Retry,
            "cannot tell whether the hunk exists; do not ghost-advance"
        );
        assert!(last_round_done(true, MatchOpen::Open));
        assert!(last_round_done(false, MatchOpen::Closed));
        assert!(
            !last_round_done(false, MatchOpen::Open),
            "still in_progress: retry finish_open_match"
        );
        assert!(
            !last_round_done(false, MatchOpen::Unknown),
            "busy mark must not leave a won fight without a room"
        );
        assert_eq!(
            after_non_final(MatchOpen::Open),
            AfterNonFinal::EndAndAdvance
        );
        assert_eq!(after_non_final(MatchOpen::Closed), AfterNonFinal::Expire);
        assert_eq!(
            after_non_final(MatchOpen::Unknown),
            AfterNonFinal::Retry,
            "busy open-status must not broadcast End before the next Hello"
        );
    }

    #[tokio::test]
    async fn try_send_or_spawn_delivers_when_buffer_is_full() {
        let (tx, mut rx) = mpsc::channel::<String>(1);
        tx.try_send("held".into()).unwrap();
        try_send_or_spawn(&tx, "hello".into());
        assert_eq!(rx.recv().await.as_deref(), Some("held"));
        let next = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("spawned send waited")
            .expect("channel open");
        assert_eq!(next, "hello");
    }
}
