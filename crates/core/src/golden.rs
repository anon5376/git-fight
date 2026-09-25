//! Frozen seed + scripted inputs. Native and WASM tests must agree on `GOLDEN_HASH`.

use crate::fight::{FightState, FighterStats, Input, ROUND_TICKS};

pub const GOLDEN_SEED: u64 = 0xD1CE_CA5E_F00D_0001;

/// Update this only when the sim itself changes, and update both test targets together.
pub const GOLDEN_HASH: u64 = 0xAA33_F0DD_F04F_7E58;

#[derive(Clone, Copy)]
pub struct InputSpan {
    pub ticks: u32,
    pub ours: u8,
    pub theirs: u8,
}

/// Scripted list of inputs. `ours`/`theirs` are `Input::as_u8` values.
pub const GOLDEN_SCRIPT: &[InputSpan] = &[
    InputSpan {
        ticks: 8,
        ours: 0,
        theirs: 3,
    },
    InputSpan {
        ticks: 1,
        ours: 1,
        theirs: 0,
    },
    InputSpan {
        ticks: 16,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 2,
        theirs: 1,
    },
    InputSpan {
        ticks: 20,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 4,
        theirs: 3,
    },
    InputSpan {
        ticks: 24,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 1,
        theirs: 0,
    },
    InputSpan {
        ticks: 16,
        ours: 0,
        theirs: 2,
    },
    InputSpan {
        ticks: 1,
        ours: 1,
        theirs: 0,
    },
    InputSpan {
        ticks: 16,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 2,
        theirs: 0,
    },
    InputSpan {
        ticks: 18,
        ours: 0,
        theirs: 3,
    },
    InputSpan {
        ticks: 1,
        ours: 4,
        theirs: 0,
    },
    InputSpan {
        ticks: 30,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 1,
        theirs: 1,
    },
    InputSpan {
        ticks: 20,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 2,
        theirs: 0,
    },
    InputSpan {
        ticks: 20,
        ours: 0,
        theirs: 0,
    },
    InputSpan {
        ticks: 1,
        ours: 1,
        theirs: 0,
    },
    InputSpan {
        ticks: 400,
        ours: 0,
        theirs: 0,
    },
];

pub fn run_golden() -> u64 {
    let stats = FighterStats {
        hp: 100,
        armor: false,
        special: true,
    };
    let mut fight = FightState::new(GOLDEN_SEED, stats, stats);
    let mut remaining = GOLDEN_SCRIPT;
    let mut span_left = remaining.first().map(|s| s.ticks).unwrap_or(0);
    for _ in 0..ROUND_TICKS {
        if remaining.is_empty() {
            fight.step(Input::None, Input::None);
        } else {
            let span = remaining[0];
            fight.step(Input::from_u8(span.ours), Input::from_u8(span.theirs));
            span_left = span_left.saturating_sub(1);
            if span_left == 0 {
                remaining = &remaining[1..];
                span_left = remaining.first().map(|s| s.ticks).unwrap_or(0);
            }
        }
        if fight.result.is_some() {
            break;
        }
    }
    fight.state_hash()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_hash_is_stable() {
        let h = run_golden();
        assert_eq!(
            h, GOLDEN_HASH,
            "update GOLDEN_HASH in crates/core/src/golden.rs to {h:#x}"
        );
        assert_eq!(run_golden(), run_golden());
    }
}
