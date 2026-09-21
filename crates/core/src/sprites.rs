//! ASCII sprites shared by the CLI and the browser canvas.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Side {
    Ours,
    Theirs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Pose {
    Idle,
    Punch,
    Kick,
    Block,
    Special,
    Hit,
    Ko,
}

pub const SPRITE_ROWS: usize = 5;
pub const SPRITE_COLS: usize = 8;

impl Pose {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Punch,
            2 => Self::Kick,
            3 => Self::Block,
            4 => Self::Special,
            5 => Self::Hit,
            6 => Self::Ko,
            _ => Self::Idle,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Idle => 0,
            Self::Punch => 1,
            Self::Kick => 2,
            Self::Block => 3,
            Self::Special => 4,
            Self::Hit => 5,
            Self::Ko => 6,
        }
    }
}

impl Side {
    pub fn from_u8(v: u8) -> Self {
        if v == 1 {
            Self::Theirs
        } else {
            Self::Ours
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::Ours => 0,
            Self::Theirs => 1,
        }
    }
}

const IDLE_OURS: [&str; SPRITE_ROWS] =
    ["  .-.   ", "  |o|   ", "  /|\\   ", "  / \\   ", "        "];
const PUNCH_OURS: [&str; SPRITE_ROWS] =
    ["  .-.   ", "  |o|-- ", "  /|    ", "  / \\   ", "        "];
const KICK_OURS: [&str; SPRITE_ROWS] = ["  .-.   ", "  |o|   ", "  /| -- ", "  /     ", "        "];
const BLOCK_OURS: [&str; SPRITE_ROWS] =
    ["  .-.   ", "  |o|#  ", "  /|#   ", "  / \\   ", "        "];
const SPECIAL_OURS: [&str; SPRITE_ROWS] =
    ["  .-.   ", "  |o|*=<", "  /|\\   ", "  / \\   ", "        "];
const HIT_OURS: [&str; SPRITE_ROWS] =
    ["  .-.   ", "  |x|   ", "  /|\\   ", "  / \\   ", "        "];
const KO_OURS: [&str; SPRITE_ROWS] = ["        ", "        ", " .-.    ", "-|o|    ", "/ |     "];

const IDLE_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", "   |o|  ", "   /|\\  ", "   / \\  ", "        "];
const PUNCH_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", " --|o|  ", "    |\\  ", "   / \\  ", "        "];
const KICK_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", "   |o|  ", " -- |\\  ", "     \\  ", "        "];
const BLOCK_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", "  #|o|  ", "   #|\\  ", "   / \\  ", "        "];
const SPECIAL_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", ">=*|o|  ", "   /|\\  ", "   / \\  ", "        "];
const HIT_THEIRS: [&str; SPRITE_ROWS] =
    ["   .-.  ", "   |x|  ", "   /|\\  ", "   / \\  ", "        "];
const KO_THEIRS: [&str; SPRITE_ROWS] =
    ["        ", "        ", "    .-. ", "    |o|-", "     | \\"];

pub fn sprite(side: Side, pose: Pose) -> [&'static str; SPRITE_ROWS] {
    match (side, pose) {
        (Side::Ours, Pose::Idle) => IDLE_OURS,
        (Side::Ours, Pose::Punch) => PUNCH_OURS,
        (Side::Ours, Pose::Kick) => KICK_OURS,
        (Side::Ours, Pose::Block) => BLOCK_OURS,
        (Side::Ours, Pose::Special) => SPECIAL_OURS,
        (Side::Ours, Pose::Hit) => HIT_OURS,
        (Side::Ours, Pose::Ko) => KO_OURS,
        (Side::Theirs, Pose::Idle) => IDLE_THEIRS,
        (Side::Theirs, Pose::Punch) => PUNCH_THEIRS,
        (Side::Theirs, Pose::Kick) => KICK_THEIRS,
        (Side::Theirs, Pose::Block) => BLOCK_THEIRS,
        (Side::Theirs, Pose::Special) => SPECIAL_THEIRS,
        (Side::Theirs, Pose::Hit) => HIT_THEIRS,
        (Side::Theirs, Pose::Ko) => KO_THEIRS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sprites_are_fixed_size() {
        for side in [Side::Ours, Side::Theirs] {
            for pose in [
                Pose::Idle,
                Pose::Punch,
                Pose::Kick,
                Pose::Block,
                Pose::Special,
                Pose::Hit,
                Pose::Ko,
            ] {
                let rows = sprite(side, pose);
                assert_eq!(rows.len(), SPRITE_ROWS);
                for row in rows {
                    assert_eq!(row.len(), SPRITE_COLS, "{row:?}");
                    assert!(row.bytes().all(|b| b < 128), "non-ascii {row:?}");
                }
            }
            for v in 0..=6 {
                assert_eq!(Pose::from_u8(v).as_u8(), v);
            }
        }
    }
}
