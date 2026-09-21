//! JSON protocol for match rooms.

use serde::{Deserialize, Serialize};

pub const INPUT_DELAY: u32 = 3;
pub const INPUT_WINDOW: u32 = 90;
pub const DISCONNECT_SECS: u64 = 30;
pub const EXPIRE_SECS: i64 = 24 * 60 * 60;

/// URL- and `git-fight/pr-<n>-<id>`-safe. Lowercase hex (or test ids).
pub fn is_match_id(id: &str) -> bool {
    let n = id.len();
    (1..=64).contains(&n)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

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
        let ours = ours_login.is_some_and(|o| o.eq_ignore_ascii_case(login));
        let theirs = theirs_login.is_some_and(|t| t.eq_ignore_ascii_case(login));
        return match (ours, theirs) {
            (true, true) => Role::Both,
            (true, false) => Role::Ours,
            (false, true) => Role::Theirs,
            (false, false) => Role::Spectator,
        };
    }
    fn share(t: Option<&str>) -> Option<&str> {
        t.filter(|s| !s.is_empty())
    }
    match share(token) {
        Some(t) if share(ours_token) == Some(t) && share(theirs_token) == Some(t) => Role::Both,
        Some(t) if share(ours_token) == Some(t) => Role::Ours,
        Some(t) if share(theirs_token) == Some(t) => Role::Theirs,
        _ => Role::Spectator,
    }
}

/// Whether this socket may put `Input` on the room queue.
/// GitHub: session login is ours or the **current-round** theirs.
/// A later-round blamed author is a spectator until that conflict starts.
/// Local: a non-empty share token that owns a slot. Spectators never enqueue.
pub fn can_enqueue_input(
    github: bool,
    ours_login: Option<&str>,
    current_theirs_login: Option<&str>,
    login: Option<&str>,
    token: Option<&str>,
    ours_token: Option<&str>,
    theirs_token: Option<&str>,
) -> bool {
    if github {
        let Some(login) = login.filter(|s| !s.is_empty()) else {
            return false;
        };
        return ours_login.is_some_and(|o| o.eq_ignore_ascii_case(login))
            || current_theirs_login.is_some_and(|t| t.eq_ignore_ascii_case(login));
    }
    role_for(false, None, None, login, token, ours_token, theirs_token) != Role::Spectator
}

/// `GET /ws` Error body for a match that is not a room.
pub fn closed_ws_message<'a>(status: &'a str, abort_reason: Option<&str>) -> Option<&'a str> {
    match status {
        "expired" | "aborted" | "finished" => {
            if abort_reason == Some("outdated") {
                Some("outdated")
            } else {
                Some(status)
            }
        }
        _ => None,
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
        assert_eq!(
            role_for(false, None, None, None, Some(""), Some(""), Some("")),
            Role::Spectator,
            "empty share tokens cannot claim a slot"
        );
        assert_eq!(
            role_for(
                false,
                None,
                None,
                None,
                Some("ours-token"),
                Some("ours-token"),
                Some("theirs-token")
            ),
            Role::Ours
        );
        assert!(
            can_enqueue_input(
                true,
                Some("alice"),
                Some("bob"),
                Some("alice"),
                None,
                None,
                None
            ),
            "ours login may enqueue"
        );
        assert!(
            can_enqueue_input(
                true,
                Some("alice"),
                Some("carol"),
                Some("carol"),
                None,
                None,
                None
            ),
            "current-round theirs may enqueue"
        );
        assert!(
            !can_enqueue_input(
                true,
                Some("alice"),
                Some("bob"),
                Some("carol"),
                None,
                None,
                None
            ),
            "later-round theirs must not fill the queue before their conflict"
        );
        assert!(
            !can_enqueue_input(
                true,
                Some("alice"),
                Some("carol"),
                Some("bob"),
                None,
                None,
                None
            ),
            "previous-round theirs is a spectator after their conflict"
        );
        assert!(
            !can_enqueue_input(
                true,
                Some("alice"),
                Some("bob"),
                Some("dave"),
                Some("ours-token"),
                Some("o"),
                Some("t")
            ),
            "GitHub spectator Input never reaches the room queue"
        );
        assert!(
            !can_enqueue_input(true, Some("alice"), Some("bob"), None, None, None, None),
            "anonymous GitHub socket cannot enqueue"
        );
        assert!(
            !can_enqueue_input(false, None, None, None, Some(""), Some(""), Some("")),
            "empty local token cannot enqueue"
        );
        assert!(can_enqueue_input(
            false,
            None,
            None,
            None,
            Some("ours-token"),
            Some("ours-token"),
            Some("theirs-token")
        ));
        assert_eq!(
            role_for(
                true,
                Some("Alice"),
                Some("Bob"),
                Some("alice"),
                None,
                None,
                None
            ),
            Role::Ours,
            "GitHub logins are case-insensitive"
        );
        assert_eq!(
            role_for(
                true,
                Some("alice"),
                Some("BOB"),
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
                Some("Alice"),
                Some("ALICE"),
                Some("alice"),
                None,
                None,
                None
            ),
            Role::Both
        );
        assert!(
            can_enqueue_input(
                true,
                Some("Alice"),
                Some("Carol"),
                Some("carol"),
                None,
                None,
                None
            ),
            "current-round theirs matches regardless of case"
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
            path: "lib.rs".into(),
            hunk_index: 1,
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
        assert_eq!(v["path"], "lib.rs");
        assert_eq!(v["hunk_index"], 1);
        assert_eq!(v["ticks"], serde_json::json!([[0, 1, 0], [1, 0, 2]]));
    }

    #[test]
    fn match_ids_are_url_and_ref_safe() {
        assert!(is_match_id("deadbeef"));
        assert!(is_match_id("match1"));
        assert!(is_match_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"));
        assert!(!is_match_id(""));
        assert!(!is_match_id("../main"));
        assert!(!is_match_id("MAIN"));
        assert!(!is_match_id(&"a".repeat(65)));
        assert!(!is_match_id("x/y"));
        assert!(!is_match_id("id;drop"));
    }

    #[test]
    fn play_caps() {
        assert_eq!(INPUT_DELAY, 3);
        assert_eq!(DISCONNECT_SECS, 30);
        assert_eq!(EXPIRE_SECS, 24 * 60 * 60);
    }

    #[test]
    fn input_round_is_optional_on_the_wire() {
        let v: ClientMsg =
            serde_json::from_str(r#"{"type":"input","tick":3,"buttons":1}"#).unwrap();
        match v {
            ClientMsg::Input {
                tick,
                buttons,
                round,
                theirs,
            } => {
                assert_eq!(tick, 3);
                assert_eq!(buttons, 1);
                assert_eq!(round, None);
                assert_eq!(theirs, None);
            }
        }
        let v: ClientMsg =
            serde_json::from_str(r#"{"type":"input","tick":3,"buttons":1,"round":1}"#).unwrap();
        match v {
            ClientMsg::Input { round, .. } => assert_eq!(round, Some(1)),
        }
    }

    #[test]
    fn closed_ws_messages() {
        assert_eq!(closed_ws_message("finished", None), Some("finished"));
        assert_eq!(
            closed_ws_message("expired", Some("expired")),
            Some("expired")
        );
        assert_eq!(
            closed_ws_message("aborted", Some("too_many")),
            Some("aborted")
        );
        assert_eq!(
            closed_ws_message("aborted", Some("outdated")),
            Some("outdated")
        );
        assert_eq!(closed_ws_message("in_progress", None), None);
        assert_eq!(closed_ws_message("pending", None), None);
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
        /// Hello round. A leftover Input from a finished round is dropped.
        #[serde(default)]
        round: Option<u32>,
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
        path: String,
        hunk_index: u32,
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
        path: String,
        hunk_index: u32,
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
