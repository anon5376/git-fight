use crate::db::{self, MatchRow};
use crate::protocol::{
    split_hash, split_seed, Role, ServerMsg, DISCONNECT_SECS, INPUT_DELAY, INPUT_WINDOW,
};
use chrono::{DateTime, Utc};
use git_fight_core::{FightState, FighterStats, Input, RoundResult, Side};
use sqlx::SqlitePool;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

pub enum RoomEvent {
    Join {
        role: Role,
        tx: mpsc::Sender<String>,
    },
    Leave {
        role: Role,
    },
    Input {
        role: Role,
        tick: u32,
        buttons: u8,
    },
    Shutdown,
}

struct Slot {
    kind_cpu: bool,
    seen: bool,
    tx: Option<mpsc::Sender<String>>,
    disconnected_at: Option<Instant>,
}

pub struct RoomSettings {
    pub instant: bool,
    pub disconnect: Duration,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            instant: false,
            disconnect: Duration::from_secs(DISCONNECT_SECS),
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
    let stats = FighterStats::default();
    let mut sim = FightState::new(seed, stats, stats);
    let mut next_tick = 0u32;
    let mut log: Vec<(u32, u8, u8)> = Vec::new();
    if let Ok(inputs) = db::load_inputs(&pool, &row.id).await {
        for (tick, ours, theirs) in &inputs {
            if *tick == next_tick && sim.result.is_none() {
                sim.step(Input::from_u8(*ours), Input::from_u8(*theirs));
                log.push((*tick, *ours, *theirs));
                next_tick = next_tick.saturating_add(1);
            }
        }
    }

    let mut ours = Slot {
        kind_cpu: row.ours_kind == "cpu",
        seen: row.ours_kind == "cpu",
        tx: None,
        disconnected_at: None,
    };
    let mut theirs = Slot {
        kind_cpu: row.theirs_kind == "cpu",
        seen: row.theirs_kind == "cpu",
        tx: None,
        disconnected_at: None,
    };
    let mut spectators: Vec<mpsc::Sender<String>> = Vec::new();
    let mut pending_ours: BTreeMap<u32, u8> = BTreeMap::new();
    let mut pending_theirs: BTreeMap<u32, u8> = BTreeMap::new();
    let mut started_at: Option<Instant> = None;
    let mut done = sim.result.is_some() || row.status == "finished" || row.status == "expired";
    let id = row.id.clone();
    let ours_name = row.ours_name.clone();
    let theirs_name = row.theirs_name.clone();
    let expires_at = parse_rfc3339(&row.expires_at);

    let mut clock = tokio::time::interval(Duration::from_millis(1000 / 30));
    clock.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            ev = rx.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    RoomEvent::Shutdown => {
                        if !done {
                            expire_now(&pool, &id, &ours, &theirs, &spectators).await;
                        }
                        break;
                    }
                    RoomEvent::Join { role, tx } => {
                        let confirmed = if next_tick == 0 { -1 } else { next_tick as i32 - 1 };
                        let (seed_lo, seed_hi) = split_seed(seed);
                        let hello = ServerMsg::Hello {
                            match_id: id.clone(),
                            seed_lo,
                            seed_hi,
                            input_delay: delay,
                            your_role: role.as_str().to_string(),
                            ours: ours_name.clone(),
                            theirs: theirs_name.clone(),
                            round: 0,
                            confirmed_tick: confirmed,
                        };
                        let _ = tx.send(encode(&hello)).await;
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
                            let _ = tx.send(encode(&end_msg(&sim, result))).await;
                        }
                        match role {
                            Role::Ours => {
                                ours.seen = true;
                                ours.tx = Some(tx);
                                ours.disconnected_at = None;
                            }
                            Role::Theirs => {
                                theirs.seen = true;
                                theirs.tx = Some(tx);
                                theirs.disconnected_at = None;
                            }
                            Role::Spectator => spectators.push(tx),
                        }
                        if !done && ours.seen && theirs.seen && started_at.is_none() {
                            started_at = Some(Instant::now());
                            let _ = db::set_status(
                                &pool, &id, "in_progress", true, false, None, None,
                            )
                            .await;
                        }
                    }
                    RoomEvent::Leave { role } => match role {
                        Role::Ours => {
                            ours.tx = None;
                            if !done && ours.seen && !ours.kind_cpu {
                                ours.disconnected_at = Some(Instant::now());
                            }
                        }
                        Role::Theirs => {
                            theirs.tx = None;
                            if !done && theirs.seen && !theirs.kind_cpu {
                                theirs.disconnected_at = Some(Instant::now());
                            }
                        }
                        Role::Spectator => {}
                    },
                    RoomEvent::Input {
                        role,
                        tick,
                        buttons,
                    } => {
                        if done || sim.result.is_some() {
                            continue;
                        }
                        if tick < next_tick || tick > next_tick.saturating_add(INPUT_WINDOW) {
                            continue;
                        }
                        match role {
                            Role::Ours => {
                                pending_ours.entry(tick).or_insert(buttons);
                            }
                            Role::Theirs => {
                                pending_theirs.entry(tick).or_insert(buttons);
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
                            expire_now(&pool, &id, &ours, &theirs, &spectators).await;
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
            spectators: &mut spectators,
            pool: &pool,
            id: &id,
            started_at,
            instant: settings.instant,
            disconnect: settings.disconnect,
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
    spectators: &'a mut Vec<mpsc::Sender<String>>,
    pool: &'a SqlitePool,
    id: &'a str,
    started_at: Option<Instant>,
    instant: bool,
    disconnect: Duration,
}

async fn advance(a: Advance<'_>) -> bool {
    if let Some(result) = a.sim.result {
        return finish(a.sim, a.pool, a.id, a.ours, a.theirs, a.spectators, result).await;
    }

    if let Some(at) = a.ours.disconnected_at {
        if at.elapsed() >= a.disconnect {
            a.sim.forfeit(Side::Ours);
            return finish(
                a.sim,
                a.pool,
                a.id,
                a.ours,
                a.theirs,
                a.spectators,
                RoundResult::Theirs,
            )
            .await;
        }
    }
    if let Some(at) = a.theirs.disconnected_at {
        if at.elapsed() >= a.disconnect {
            a.sim.forfeit(Side::Theirs);
            return finish(
                a.sim,
                a.pool,
                a.id,
                a.ours,
                a.theirs,
                a.spectators,
                RoundResult::Ours,
            )
            .await;
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
            if let Some(start) = a.started_at {
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
            if let Some(start) = a.started_at {
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
        let _ = db::insert_input(a.pool, a.id, n, ours_btn, theirs_btn).await;
        let msg = encode(&ServerMsg::Tick {
            n,
            ours: ours_btn,
            theirs: theirs_btn,
        });
        broadcast(a.ours, a.theirs, a.spectators, &msg).await;
        if n % 10 == 9 || a.sim.result.is_some() {
            let (lo, hi) = split_hash(a.sim.state_hash());
            let hash = encode(&ServerMsg::Hash {
                n: a.sim.tick,
                hi,
                lo,
            });
            broadcast(a.ours, a.theirs, a.spectators, &hash).await;
        }
    }
    if let Some(result) = a.sim.result {
        return finish(a.sim, a.pool, a.id, a.ours, a.theirs, a.spectators, result).await;
    }
    false
}

async fn finish(
    sim: &FightState,
    pool: &SqlitePool,
    id: &str,
    ours: &Slot,
    theirs: &Slot,
    spectators: &[mpsc::Sender<String>],
    result: RoundResult,
) -> bool {
    let (lo, hi) = split_hash(sim.state_hash());
    let hash_s = format!("{hi:08x}{lo:08x}");
    let _ = db::set_status(pool, id, "finished", true, true, Some(&hash_s), None).await;
    let msg = encode(&end_msg(sim, result));
    broadcast(ours, theirs, spectators, &msg).await;
    true
}

async fn expire_now(
    pool: &SqlitePool,
    id: &str,
    ours: &Slot,
    theirs: &Slot,
    spectators: &[mpsc::Sender<String>],
) {
    let _ = db::set_status(pool, id, "expired", false, true, None, Some("expired")).await;
    let msg = encode(&ServerMsg::Error {
        message: "expired".into(),
    });
    broadcast(ours, theirs, spectators, &msg).await;
}

fn end_msg(sim: &FightState, result: RoundResult) -> ServerMsg {
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
    }
}

async fn broadcast(ours: &Slot, theirs: &Slot, spectators: &[mpsc::Sender<String>], msg: &str) {
    if let Some(tx) = &ours.tx {
        let _ = tx.send(msg.to_string()).await;
    }
    if let Some(tx) = &theirs.tx {
        let _ = tx.send(msg.to_string()).await;
    }
    for tx in spectators.iter() {
        let _ = tx.send(msg.to_string()).await;
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
