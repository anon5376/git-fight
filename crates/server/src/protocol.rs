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

/// Slot from the session login (GitHub matches) or share token (local matches).
/// `theirs_login` is the right-side identity for the **current** round.
#[allow(clippy::too_many_arguments)]
pub fn role_for(
    github: bool,
    ours_login: Option<&str>,
    theirs_login: Option<&str>,
    login: Option<&str>,
    token: Option<&str>,
    ours_token: Option<&str>,
    theirs_token: Option<&str>,
) -> Role {
    if github {
        let Some(login) = login else {
            return Role::Spectator;
        };
        let ours = ours_login == Some(login);
        let theirs = theirs_login == Some(login);
        return match (ours, theirs) {
            (true, true) => Role::Both,
            (true, false) => Role::Ours,
            (false, true) => Role::Theirs,
            (false, false) => Role::Spectator,
        };
    }
    match token {
        Some(t) if ours_token == Some(t) && theirs_token == Some(t) => Role::Both,
        Some(t) if ours_token == Some(t) => Role::Ours,
        Some(t) if theirs_token == Some(t) => Role::Theirs,
        _ => Role::Spectator,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_roles_follow_this_round_theirs() {
        assert_eq!(
            role_for(
                true,
                Some("alice"),
                Some("bob"),
                Some("bob"),
                None,
                None,
                None
            ),
            Role::Theirs
        );
        assert_eq!(
            role_for(
                true,
                Some("alice"),
                Some("carol"),
                Some("bob"),
                None,
                None,
                None
            ),
            Role::Spectator
        );
        assert_eq!(
            role_for(
                true,
                Some("alice"),
                Some("alice"),
                Some("alice"),
                None,
                None,
                None
            ),
            Role::Both
        );
        assert_eq!(
            role_for(
                true,
                Some("alice"),
                Some("bob"),
                None,
                Some("ours-token"),
                Some("o"),
                Some("t")
            ),
            Role::Spectator
        );
        assert_eq!(round_seed(7, 0), 7);
        assert_eq!(round_seed(7, 1), 14);
    }

    #[test]
    fn snapshot_wire_includes_tick_log() {
        let msg = ServerMsg::Snapshot {
            seed_lo: 1,
            seed_hi: 0,
            round: 2,
            confirmed_tick: 1,
            ours_hp: 100,
            ours_armor: false,
            ours_special: false,
            theirs_hp: 100,
            theirs_armor: false,
            theirs_special: false,
            ticks: vec![(0, 1, 0), (1, 0, 2)],
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "snapshot");
        assert_eq!(v["round"], 2);
        assert_eq!(v["ticks"], serde_json::json!([[0, 1, 0], [1, 0, 2]]));
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
        you_are: String,
        ours: String,
        theirs: String,
        round: u32,
        total_rounds: u32,
        confirmed_tick: i32,
        ours_hp: i32,
        ours_armor: bool,
        ours_special: bool,
        theirs_hp: i32,
        theirs_armor: bool,
        theirs_special: bool,
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
    Snapshot {
        seed_lo: u32,
        seed_hi: u32,
        round: u32,
        confirmed_tick: i32,
        ours_hp: i32,
        ours_armor: bool,
        ours_special: bool,
        theirs_hp: i32,
        theirs_armor: bool,
        theirs_special: bool,
        ticks: Vec<(u32, u8, u8)>,
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

pub fn round_seed(seed: u64, round: u32) -> u64 {
    seed.wrapping_mul(u64::from(round) + 1)
}
