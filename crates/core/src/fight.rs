//! Deterministic 30-tick fighting sim. Integer math only. No HashMap.

use crate::hash::mix;
use crate::pcg32::Pcg32;
use crate::sprites::{Pose, Side};

pub const TICKS_PER_SECOND: u32 = 30;
pub const ROUND_TICKS: u32 = TICKS_PER_SECOND * 15;
pub const BLOCK_TICKS: u32 = TICKS_PER_SECOND / 2;
pub const ARENA_W: i32 = 72;
pub const FIGHTER_W: i32 = 8;
pub const START_OURS_X: i32 = 24;
pub const START_THEIRS_X: i32 = 38;

const PUNCH_STARTUP: u32 = 4;
const PUNCH_ACTIVE: u32 = 2;
const PUNCH_RECOVERY: u32 = 8;
const KICK_STARTUP: u32 = 6;
const KICK_ACTIVE: u32 = 3;
const KICK_RECOVERY: u32 = 12;
const SPECIAL_STARTUP: u32 = 8;
const SPECIAL_ACTIVE: u32 = 4;
const SPECIAL_RECOVERY: u32 = 16;

const PUNCH_DAMAGE: i32 = 10;
const KICK_DAMAGE: i32 = 16;
const SPECIAL_DAMAGE: i32 = 28;
const PUNCH_RANGE: i32 = 16;
const KICK_RANGE: i32 = 20;
const SPECIAL_RANGE: i32 = 24;
const HITSTUN: u32 = 8;
const BLOCKSTUN: u32 = 4;
const KNOCKBACK: i32 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Input {
    None,
    Punch,
    Kick,
    Block,
    Special,
}

impl Input {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Punch,
            2 => Self::Kick,
            3 => Self::Block,
            4 => Self::Special,
            _ => Self::None,
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Punch => 1,
            Self::Kick => 2,
            Self::Block => 3,
            Self::Special => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttackKind {
    Punch,
    Kick,
    Special,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Anim {
    Idle,
    Attack { kind: AttackKind, frame: u32 },
    Block,
    Hit,
    Ko,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoundResult {
    Ours,
    Theirs,
    Draw,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FighterStats {
    pub hp: i32,
    pub armor: bool,
    pub special: bool,
}

impl Default for FighterStats {
    fn default() -> Self {
        Self {
            hp: 100,
            armor: false,
            special: false,
        }
    }
}

impl FighterStats {
    pub fn clamped(hp: i32, armor: bool, special: bool) -> Self {
        let hp = hp.clamp(80, 120);
        Self { hp, armor, special }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fighter {
    pub x: i32,
    pub hp: i32,
    pub max_hp: i32,
    pub armor: bool,
    pub special: bool,
    pub guard_ticks: u32,
    pub stun_ticks: u32,
    pub vel_x: i32,
    pub anim: Anim,
    pub hit_connected: bool,
}

impl Fighter {
    fn new(x: i32, stats: FighterStats) -> Self {
        Self {
            x,
            hp: stats.hp,
            max_hp: stats.hp,
            armor: stats.armor,
            special: stats.special,
            guard_ticks: 0,
            stun_ticks: 0,
            vel_x: 0,
            anim: Anim::Idle,
            hit_connected: false,
        }
    }

    fn can_act(&self) -> bool {
        self.stun_ticks == 0 && self.hp > 0 && matches!(self.anim, Anim::Idle | Anim::Block)
    }

    pub fn pose(&self) -> Pose {
        match self.anim {
            Anim::Idle => Pose::Idle,
            Anim::Attack {
                kind: AttackKind::Punch,
                ..
            } => Pose::Punch,
            Anim::Attack {
                kind: AttackKind::Kick,
                ..
            } => Pose::Kick,
            Anim::Attack {
                kind: AttackKind::Special,
                ..
            } => Pose::Special,
            Anim::Block => Pose::Block,
            Anim::Hit => Pose::Hit,
            Anim::Ko => Pose::Ko,
        }
    }

    fn hash_into(&self, mut h: u64) -> u64 {
        h = mix(h, self.x as u64);
        h = mix(h, self.hp as u64);
        h = mix(h, self.max_hp as u64);
        h = mix(h, u64::from(self.armor));
        h = mix(h, u64::from(self.special));
        h = mix(h, u64::from(self.guard_ticks));
        h = mix(h, u64::from(self.stun_ticks));
        h = mix(h, self.vel_x as u64);
        h = mix(h, anim_tag(self.anim));
        h = mix(h, u64::from(self.hit_connected));
        h
    }
}

fn anim_tag(anim: Anim) -> u64 {
    match anim {
        Anim::Idle => 1,
        Anim::Attack { kind, frame } => {
            let k = match kind {
                AttackKind::Punch => 1,
                AttackKind::Kick => 2,
                AttackKind::Special => 3,
            };
            10 + k * 1_000 + u64::from(frame)
        }
        Anim::Block => 2,
        Anim::Hit => 3,
        Anim::Ko => 4,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FightState {
    pub tick: u32,
    pub round_ticks: u32,
    pub ours: Fighter,
    pub theirs: Fighter,
    pub rng: Pcg32,
    pub result: Option<RoundResult>,
}

impl FightState {
    pub fn new(seed: u64, ours: FighterStats, theirs: FighterStats) -> Self {
        Self {
            tick: 0,
            round_ticks: ROUND_TICKS,
            ours: Fighter::new(START_OURS_X, ours),
            theirs: Fighter::new(START_THEIRS_X, theirs),
            rng: Pcg32::new(seed),
            result: None,
        }
    }

    pub fn state_hash(&self) -> u64 {
        let mut h = mix(0x61F1_66F1, u64::from(self.tick));
        h = mix(h, u64::from(self.round_ticks));
        h = self.ours.hash_into(h);
        h = self.theirs.hash_into(h);
        h = mix(h, self.rng.state);
        h = mix(h, self.rng.inc);
        h = mix(
            h,
            match self.result {
                None => 0,
                Some(RoundResult::Ours) => 1,
                Some(RoundResult::Theirs) => 2,
                Some(RoundResult::Draw) => 3,
            },
        );
        h
    }

    /// Pick a CPU button from the current frame. Does **not** advance `rng`:
    /// lockstep clients apply the Tick buttons and must hash the same state.
    pub fn cpu_input(&self, side: Side) -> Input {
        let me = match side {
            Side::Ours => &self.ours,
            Side::Theirs => &self.theirs,
        };
        if !me.can_act() {
            return Input::None;
        }
        let foe = match side {
            Side::Ours => &self.theirs,
            Side::Theirs => &self.ours,
        };
        let dist = (self.theirs.x - self.ours.x).unsigned_abs();
        let foe_attacking = matches!(foe.anim, Anim::Attack { .. });
        let roll = self.cpu_roll(side);
        if foe_attacking && dist < 20 && roll < 6 {
            return Input::Block;
        }
        if me.special && roll == 0 {
            return Input::Special;
        }
        match roll {
            1..=3 => Input::Punch,
            4..=6 => Input::Kick,
            7 => Input::Block,
            _ => Input::None,
        }
    }

    fn cpu_roll(&self, side: Side) -> u32 {
        let mut rng = self.rng.clone();
        let side_bit = match side {
            Side::Ours => 0u64,
            Side::Theirs => 1,
        };
        rng.state = rng
            .state
            .wrapping_add(u64::from(self.tick).wrapping_mul(0x9E37_79B9_7F4A_7C15))
            .wrapping_add(side_bit.wrapping_mul(0xBF58_476D_1CE4_E5B9));
        rng.next_bounded(10)
    }

    pub fn step(&mut self, ours_in: Input, theirs_in: Input) {
        if self.result.is_some() {
            return;
        }

        tick_timers(&mut self.ours);
        tick_timers(&mut self.theirs);

        apply_input(&mut self.ours, ours_in);
        apply_input(&mut self.theirs, theirs_in);

        advance_anim(&mut self.ours);
        advance_anim(&mut self.theirs);

        let ours_hit = active_hit(&self.ours, &self.theirs, true);
        let theirs_hit = active_hit(&self.theirs, &self.ours, false);
        if let Some(kind) = ours_hit {
            self.ours.hit_connected = true;
            apply_hit(&mut self.theirs, kind, 1);
        }
        if let Some(kind) = theirs_hit {
            self.theirs.hit_connected = true;
            apply_hit(&mut self.ours, kind, -1);
        }

        apply_physics(&mut self.ours, &mut self.theirs);
        maybe_nudge(&mut self.ours, &mut self.theirs);

        self.tick = self.tick.saturating_add(1);
        self.finish_if_needed();
    }

    /// The disconnected side loses the current round immediately.
    pub fn forfeit(&mut self, side: Side) {
        if self.result.is_some() {
            return;
        }
        match side {
            Side::Ours => {
                self.ours.hp = 0;
                self.ours.anim = Anim::Ko;
                self.result = Some(RoundResult::Theirs);
            }
            Side::Theirs => {
                self.theirs.hp = 0;
                self.theirs.anim = Anim::Ko;
                self.result = Some(RoundResult::Ours);
            }
        }
    }

    fn finish_if_needed(&mut self) {
        let ours_dead = self.ours.hp <= 0;
        let theirs_dead = self.theirs.hp <= 0;
        if ours_dead || theirs_dead {
            if ours_dead {
                self.ours.hp = 0;
                self.ours.anim = Anim::Ko;
            }
            if theirs_dead {
                self.theirs.hp = 0;
                self.theirs.anim = Anim::Ko;
            }
            self.result = Some(if ours_dead && theirs_dead {
                RoundResult::Draw
            } else if ours_dead {
                RoundResult::Theirs
            } else {
                RoundResult::Ours
            });
            return;
        }
        if self.tick >= self.round_ticks {
            self.result = Some(match self.ours.hp.cmp(&self.theirs.hp) {
                core::cmp::Ordering::Greater => RoundResult::Ours,
                core::cmp::Ordering::Less => RoundResult::Theirs,
                core::cmp::Ordering::Equal => RoundResult::Draw,
            });
        }
    }
}

fn tick_timers(f: &mut Fighter) {
    if f.guard_ticks > 0 {
        f.guard_ticks -= 1;
        if f.guard_ticks == 0 && f.anim == Anim::Block && f.stun_ticks == 0 {
            f.anim = Anim::Idle;
        }
    }
    if f.stun_ticks > 0 {
        f.stun_ticks -= 1;
        if f.stun_ticks == 0 && f.anim == Anim::Hit && f.hp > 0 {
            f.anim = Anim::Idle;
        }
    }
    if f.vel_x > 0 {
        f.vel_x -= 1;
    } else if f.vel_x < 0 {
        f.vel_x += 1;
    }
}

fn apply_input(f: &mut Fighter, input: Input) {
    if !f.can_act() {
        return;
    }
    match input {
        Input::None => {}
        Input::Block => {
            f.anim = Anim::Block;
            f.guard_ticks = BLOCK_TICKS;
            f.hit_connected = false;
        }
        Input::Punch => start_attack(f, AttackKind::Punch),
        Input::Kick => start_attack(f, AttackKind::Kick),
        Input::Special => {
            if f.special {
                start_attack(f, AttackKind::Special);
            }
        }
    }
}

fn start_attack(f: &mut Fighter, kind: AttackKind) {
    f.anim = Anim::Attack { kind, frame: 0 };
    f.guard_ticks = 0;
    f.hit_connected = false;
}

fn advance_anim(f: &mut Fighter) {
    let Anim::Attack { kind, frame } = f.anim else {
        return;
    };
    let (startup, active, recovery) = frames(kind);
    let next = frame.saturating_add(1);
    if next >= startup + active + recovery {
        f.anim = Anim::Idle;
        f.hit_connected = false;
    } else {
        f.anim = Anim::Attack { kind, frame: next };
    }
}

fn frames(kind: AttackKind) -> (u32, u32, u32) {
    match kind {
        AttackKind::Punch => (PUNCH_STARTUP, PUNCH_ACTIVE, PUNCH_RECOVERY),
        AttackKind::Kick => (KICK_STARTUP, KICK_ACTIVE, KICK_RECOVERY),
        AttackKind::Special => (SPECIAL_STARTUP, SPECIAL_ACTIVE, SPECIAL_RECOVERY),
    }
}

fn damage(kind: AttackKind) -> i32 {
    match kind {
        AttackKind::Punch => PUNCH_DAMAGE,
        AttackKind::Kick => KICK_DAMAGE,
        AttackKind::Special => SPECIAL_DAMAGE,
    }
}

fn range(kind: AttackKind) -> i32 {
    match kind {
        AttackKind::Punch => PUNCH_RANGE,
        AttackKind::Kick => KICK_RANGE,
        AttackKind::Special => SPECIAL_RANGE,
    }
}

fn active_hit(
    attacker: &Fighter,
    defender: &Fighter,
    attacker_is_ours: bool,
) -> Option<AttackKind> {
    if attacker.hit_connected || attacker.hp <= 0 {
        return None;
    }
    let Anim::Attack { kind, frame } = attacker.anim else {
        return None;
    };
    let (startup, active, _) = frames(kind);
    if frame < startup || frame >= startup + active {
        return None;
    }
    let dist = if attacker_is_ours {
        defender.x - attacker.x
    } else {
        attacker.x - defender.x
    };
    if dist >= 0 && dist <= range(kind) {
        Some(kind)
    } else {
        None
    }
}

fn apply_hit(defender: &mut Fighter, kind: AttackKind, knockback_dir: i32) {
    if defender.hp <= 0 {
        return;
    }
    let blocking = defender.guard_ticks > 0 || defender.anim == Anim::Block;
    if blocking {
        defender.stun_ticks = BLOCKSTUN;
        defender.anim = Anim::Block;
        return;
    }
    let mut dmg = damage(kind);
    if defender.armor {
        dmg = dmg * 9 / 10;
    }
    defender.hp -= dmg;
    if defender.hp < 0 {
        defender.hp = 0;
    }
    defender.stun_ticks = HITSTUN;
    defender.vel_x += knockback_dir * KNOCKBACK;
    defender.anim = Anim::Hit;
    defender.guard_ticks = 0;
    defender.hit_connected = false;
}

fn apply_physics(ours: &mut Fighter, theirs: &mut Fighter) {
    ours.x += ours.vel_x.signum();
    theirs.x += theirs.vel_x.signum();
    ours.x = ours.x.clamp(0, ARENA_W - FIGHTER_W);
    theirs.x = theirs.x.clamp(0, ARENA_W - FIGHTER_W);
    if ours.x + FIGHTER_W > theirs.x {
        let overlap = ours.x + FIGHTER_W - theirs.x;
        let push = (overlap + 1) / 2;
        ours.x -= push;
        theirs.x += push;
        ours.x = ours.x.clamp(0, ARENA_W - FIGHTER_W);
        theirs.x = theirs.x.clamp(0, ARENA_W - FIGHTER_W);
    }
}

fn maybe_nudge(ours: &mut Fighter, theirs: &mut Fighter) {
    if !matches!(ours.anim, Anim::Idle) || !matches!(theirs.anim, Anim::Idle) {
        return;
    }
    let dist = theirs.x - ours.x;
    if dist > 22 {
        ours.x += 1;
        theirs.x -= 1;
    } else if dist < 12 {
        ours.x -= 1;
        theirs.x += 1;
    }
    ours.x = ours.x.clamp(0, ARENA_W - FIGHTER_W);
    theirs.x = theirs.x.clamp(0, ARENA_W - FIGHTER_W);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_until_actable(f: &mut FightState) {
        for _ in 0..40 {
            if f.ours.can_act() && f.theirs.can_act() {
                return;
            }
            f.step(Input::None, Input::None);
        }
    }

    #[test]
    fn punch_deals_damage() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.step(Input::Punch, Input::None);
        for _ in 0..20 {
            f.step(Input::None, Input::None);
        }
        assert!(f.theirs.hp < f.theirs.max_hp, "hp {}", f.theirs.hp);
        assert_eq!(f.ours.hp, f.ours.max_hp);
    }

    #[test]
    fn block_prevents_damage() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.step(Input::Punch, Input::Block);
        for _ in 0..20 {
            f.step(Input::None, Input::Block);
        }
        assert_eq!(f.theirs.hp, f.theirs.max_hp);
    }

    #[test]
    fn armor_reduces_damage() {
        let mut bare = FightState::new(1, FighterStats::default(), FighterStats::default());
        let armored = FighterStats {
            hp: 100,
            armor: true,
            special: false,
        };
        let mut tank = FightState::new(1, FighterStats::default(), armored);
        for fight in [&mut bare, &mut tank] {
            fight.step(Input::Punch, Input::None);
            for _ in 0..20 {
                fight.step(Input::None, Input::None);
            }
        }
        assert!(tank.theirs.hp > bare.theirs.hp);
        assert_eq!(bare.theirs.max_hp - bare.theirs.hp, PUNCH_DAMAGE);
        assert_eq!(tank.theirs.max_hp - tank.theirs.hp, PUNCH_DAMAGE * 9 / 10);
    }

    #[test]
    fn locked_special_is_ignored() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.step(Input::Special, Input::None);
        assert_eq!(f.ours.anim, Anim::Idle);
    }

    #[test]
    fn unlocked_special_starts() {
        let stats = FighterStats {
            hp: 100,
            armor: false,
            special: true,
        };
        let mut f = FightState::new(1, stats, stats);
        f.step(Input::Special, Input::None);
        assert!(matches!(
            f.ours.anim,
            Anim::Attack {
                kind: AttackKind::Special,
                ..
            }
        ));
    }

    #[test]
    fn timeout_higher_hp_wins() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.ours.hp = 80;
        f.theirs.hp = 40;
        f.tick = ROUND_TICKS - 1;
        f.step(Input::None, Input::None);
        assert_eq!(f.result, Some(RoundResult::Ours));
    }

    #[test]
    fn timeout_tie_is_draw() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.tick = ROUND_TICKS - 1;
        f.step(Input::None, Input::None);
        assert_eq!(f.result, Some(RoundResult::Draw));
    }

    #[test]
    fn ko_sets_winner() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.theirs.hp = 1;
        idle_until_actable(&mut f);
        f.theirs.x = f.ours.x + 8;
        f.step(Input::Punch, Input::None);
        for _ in 0..20 {
            f.step(Input::None, Input::None);
            if f.result.is_some() {
                break;
            }
        }
        assert_eq!(f.result, Some(RoundResult::Ours));
        assert_eq!(f.theirs.hp, 0);
    }

    #[test]
    fn same_seed_and_inputs_same_hash() {
        let run = || {
            let mut f = FightState::new(0x1111, FighterStats::default(), FighterStats::default());
            for t in 0..80 {
                let a = if t % 14 == 0 {
                    Input::Punch
                } else {
                    Input::None
                };
                let b = if t % 19 == 0 {
                    Input::Kick
                } else {
                    Input::None
                };
                f.step(a, b);
            }
            f.state_hash()
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn cpu_is_deterministic() {
        let mut a = FightState::new(7, FighterStats::default(), FighterStats::default());
        let mut b = FightState::new(7, FighterStats::default(), FighterStats::default());
        for _ in 0..60 {
            let ia = a.cpu_input(Side::Theirs);
            let ib = b.cpu_input(Side::Theirs);
            assert_eq!(ia, ib);
            a.step(Input::Punch, ia);
            b.step(Input::Punch, ib);
        }
        assert_eq!(a.state_hash(), b.state_hash());
    }

    #[test]
    fn cpu_choice_does_not_change_hash() {
        let f = FightState::new(9, FighterStats::default(), FighterStats::default());
        let before = f.state_hash();
        let _ = f.cpu_input(Side::Theirs);
        let _ = f.cpu_input(Side::Ours);
        assert_eq!(before, f.state_hash());
    }

    #[test]
    fn recorded_cpu_buttons_match_on_a_second_sim() {
        let mut server = FightState::new(9, FighterStats::default(), FighterStats::default());
        let mut client = FightState::new(9, FighterStats::default(), FighterStats::default());
        for _ in 0..80 {
            let cpu = server.cpu_input(Side::Theirs);
            assert_eq!(server.state_hash(), client.state_hash());
            server.step(Input::Punch, cpu);
            client.step(Input::Punch, cpu);
            assert_eq!(server.state_hash(), client.state_hash());
        }
    }

    #[test]
    fn forfeit_awards_the_other_side() {
        let mut f = FightState::new(1, FighterStats::default(), FighterStats::default());
        f.forfeit(Side::Ours);
        assert_eq!(f.result, Some(RoundResult::Theirs));
        assert_eq!(f.ours.hp, 0);
    }
}
