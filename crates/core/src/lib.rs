//! Conflict engine and fight simulation. Pure: no I/O, integer math, no HashMap.

#![forbid(unsafe_code)]

pub mod conflict;
pub mod demo;
pub mod fight;
pub mod golden;
pub mod hash;
pub mod pcg32;
pub mod sprites;

pub use conflict::{ConflictFile, ParseError, Pick};
pub use demo::{demo_file, resolve_demo, DEMO_CONFLICT};
pub use fight::{
    FightState, FighterStats, Input, RoundResult, ARENA_W, ROUND_TICKS, TICKS_PER_SECOND,
};
pub use golden::{run_golden, GOLDEN_HASH, GOLDEN_SEED};
pub use pcg32::Pcg32;
pub use sprites::{sprite, Pose, Side, SPRITE_COLS, SPRITE_ROWS};
