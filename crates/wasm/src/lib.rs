use git_fight_core::{
    resolve_demo, sprite, FightState, FighterStats, Input, Pick, Pose, RoundResult, Side,
    DEMO_CONFLICT, ROUND_TICKS, SPRITE_COLS, SPRITE_ROWS, TICKS_PER_SECOND,
};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn golden_hash_hi() -> u32 {
    (git_fight_core::run_golden() >> 32) as u32
}

#[wasm_bindgen]
pub fn golden_hash_lo() -> u32 {
    git_fight_core::run_golden() as u32
}

#[wasm_bindgen]
pub fn ticks_per_second() -> u32 {
    TICKS_PER_SECOND
}

#[wasm_bindgen]
pub fn round_ticks() -> u32 {
    ROUND_TICKS
}

#[wasm_bindgen]
pub fn arena_width() -> i32 {
    git_fight_core::ARENA_W
}

#[wasm_bindgen]
pub fn sprite_rows() -> u32 {
    SPRITE_ROWS as u32
}

#[wasm_bindgen]
pub fn sprite_cols() -> u32 {
    SPRITE_COLS as u32
}

#[wasm_bindgen]
pub fn sprite_row(side: u8, pose: u8, row: u8) -> String {
    let lines = sprite(Side::from_u8(side), Pose::from_u8(pose));
    let idx = usize::from(row).min(SPRITE_ROWS.saturating_sub(1));
    lines[idx].to_string()
}

#[wasm_bindgen]
pub fn demo_conflict() -> String {
    DEMO_CONFLICT.to_string()
}

#[wasm_bindgen]
pub fn demo_resolve(winner: u8) -> String {
    let pick = match winner {
        1 => Pick::Theirs,
        _ => Pick::Ours,
    };
    resolve_demo(pick)
}

fn stats(hp: i32) -> FighterStats {
    FighterStats {
        hp,
        armor: false,
        special: true,
    }
}

#[wasm_bindgen]
pub struct WasmFight {
    state: FightState,
}

#[wasm_bindgen]
impl WasmFight {
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u32) -> WasmFight {
        WasmFight::with_hp(seed, 100, 100)
    }

    pub fn with_hp(seed: u32, ours_hp: i32, theirs_hp: i32) -> WasmFight {
        WasmFight::with_seed_hp(seed, 0, ours_hp, theirs_hp)
    }

    /// Same stats the server uses (`FighterStats::default`).
    pub fn from_seed(seed_lo: u32, seed_hi: u32) -> WasmFight {
        let seed = (u64::from(seed_hi) << 32) | u64::from(seed_lo);
        WasmFight {
            state: FightState::new(seed, FighterStats::default(), FighterStats::default()),
        }
    }

    pub fn with_seed_hp(seed_lo: u32, seed_hi: u32, ours_hp: i32, theirs_hp: i32) -> WasmFight {
        let seed = (u64::from(seed_hi) << 32) | u64::from(seed_lo);
        WasmFight {
            state: FightState::new(seed, stats(ours_hp), stats(theirs_hp)),
        }
    }

    pub fn step(&mut self, ours: u8, theirs: u8) {
        self.state
            .step(Input::from_u8(ours), Input::from_u8(theirs));
    }

    pub fn cpu_input(&mut self, side: u8) -> u8 {
        self.state.cpu_input(Side::from_u8(side)).as_u8()
    }

    pub fn tick(&self) -> u32 {
        self.state.tick
    }

    /// `-1` fighting, `0` ours, `1` theirs, `2` draw.
    pub fn result(&self) -> i32 {
        match self.state.result {
            None => -1,
            Some(RoundResult::Ours) => 0,
            Some(RoundResult::Theirs) => 1,
            Some(RoundResult::Draw) => 2,
        }
    }

    pub fn ours_hp(&self) -> i32 {
        self.state.ours.hp
    }

    pub fn theirs_hp(&self) -> i32 {
        self.state.theirs.hp
    }

    pub fn ours_max_hp(&self) -> i32 {
        self.state.ours.max_hp
    }

    pub fn theirs_max_hp(&self) -> i32 {
        self.state.theirs.max_hp
    }

    pub fn ours_x(&self) -> i32 {
        self.state.ours.x
    }

    pub fn theirs_x(&self) -> i32 {
        self.state.theirs.x
    }

    pub fn ours_pose(&self) -> u8 {
        self.state.ours.pose().as_u8()
    }

    pub fn theirs_pose(&self) -> u8 {
        self.state.theirs.pose().as_u8()
    }

    pub fn state_hash_hi(&self) -> u32 {
        (self.state.state_hash() >> 32) as u32
    }

    pub fn state_hash_lo(&self) -> u32 {
        self.state.state_hash() as u32
    }
}

#[cfg(all(test, target_arch = "wasm32"))]
mod tests {
    use super::WasmFight;
    use git_fight_core::{run_golden, GOLDEN_HASH};
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen_test]
    fn golden_matches_native_constant() {
        assert_eq!(run_golden(), GOLDEN_HASH);
        assert_eq!(run_golden(), run_golden());
    }

    #[wasm_bindgen_test]
    fn wasm_fight_steps() {
        let mut fight = WasmFight::new(1);
        fight.step(1, 0);
        assert_eq!(fight.tick(), 1);
        assert_eq!(fight.result(), -1);
    }
}
