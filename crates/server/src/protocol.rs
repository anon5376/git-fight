//! JSON protocol for match rooms.

use serde::{Deserialize, Serialize};

pub const INPUT_DELAY: u32 = 3;
pub const INPUT_WINDOW: u32 = 90;
pub const DISCONNECT_SECS: u64 = 30;
pub const EXPIRE_SECS: i64 = 24 * 60 * 60;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    Ours,
    Theirs,
    Both,
    Spectator,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::Ours => "ours",
            Role::Theirs => "theirs",
            Role::Both => "both",
            Role::Spectator => "spectator",
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Input {
        tick: u32,
        buttons: u8,
        #[serde(default)]
        theirs: Option<u8>,
    },
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello {
        match_id: String,
        seed_lo: u32,
        seed_hi: u32,
        input_delay: u32,
        your_role: String,
        ours: String,
        theirs: String,
        round: u32,
        total_rounds: u32,
        confirmed_tick: i32,
    },
    Tick {
        n: u32,
        ours: u8,
        theirs: u8,
    },
    Hash {
        n: u32,
        hi: u32,
        lo: u32,
    },
    End {
        result: i32,
        hash_hi: u32,
        hash_lo: u32,
        tick: u32,
        round: u32,
        match_over: bool,
    },
    Error {
        message: String,
    },
}

pub fn split_seed(seed: u64) -> (u32, u32) {
    (seed as u32, (seed >> 32) as u32)
}

pub fn join_seed(lo: u32, hi: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

pub fn split_hash(h: u64) -> (u32, u32) {
    (h as u32, (h >> 32) as u32)
}
