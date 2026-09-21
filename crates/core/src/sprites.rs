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
        }
    }
}
