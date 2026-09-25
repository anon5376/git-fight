use std::io::{self, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use git_fight_core::{
    sprite, ConflictFile, FightState, Input, Pick, RoundResult, Side, ARENA_W, SPRITE_COLS,
    SPRITE_ROWS, TICKS_PER_SECOND,
};

use crate::stats::NamedFighter;

const BG: Color = Color::Rgb {
    r: 0x0A,
    g: 0x0A,
    b: 0x0B,
};
const OURS: Color = Color::Rgb {
    r: 0xEE,
    g: 0xEE,
    b: 0xEA,
};
const THEIRS: Color = Color::Rgb {
    r: 0xFF,
    g: 0x4A,
    b: 0x1C,
};

struct RawGuard;

impl RawGuard {
    fn enter() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| e.to_string())?;
        execute!(io::stdout(), EnterAlternateScreen, Hide).map_err(|e| e.to_string())?;
        Ok(Self)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy)]
enum Mode {
    VsCpu { human: Side },
    TwoPlayer,
}

pub fn fight_hunks(
    file: &ConflictFile,
    path: &Path,
    ours: NamedFighter,
    theirs: NamedFighter,
) -> Result<Option<Vec<Option<Pick>>>, String> {
    if !stdout_is_tty() {
        return Err("need a terminal, or pass --pick / --no-fight".into());
    }
    let _raw = RawGuard::enter()?;
    let Some(mode) = select_mode()? else {
        return Ok(None);
    };
    let n = file.hunk_count();
    let mut picks = vec![None; n];
    for (round, pick) in picks.iter_mut().enumerate() {
        match play_round(file, path, round, n, &ours, &theirs, mode)? {
            RoundChoice::Quit => return Ok(None),
            RoundChoice::Skip => *pick = None,
            RoundChoice::Winner(p) => *pick = Some(p),
        }
    }
    Ok(Some(picks))
}

enum RoundChoice {
    Winner(Pick),
    Skip,
    Quit,
}

fn play_round(
    file: &ConflictFile,
    path: &Path,
    round: usize,
    total: usize,
    ours: &NamedFighter,
    theirs: &NamedFighter,
    mode: Mode,
) -> Result<RoundChoice, String> {
    let seed = 0x0F16_u64
        .wrapping_mul(round as u64 + 1)
        .wrapping_add(path_seed(path))
        .wrapping_add(file.ours(round).len() as u64);
    let mut fight = FightState::new(seed, ours.stats, theirs.stats);
    let tick_len = Duration::from_millis(1000 / u64::from(TICKS_PER_SECOND));
    let mut last = Instant::now();
    let mut leftover = Duration::ZERO;
    let mut pending_ours = Input::None;
    let mut pending_theirs = Input::None;

    loop {
        while event::poll(Duration::from_millis(0)).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                if key.kind == event::KeyEventKind::Release {
                    continue;
                }
                match key_command(key) {
                    Command::Quit => return Ok(RoundChoice::Quit),
                    Command::Skip => return Ok(RoundChoice::Skip),
                    Command::Input { ours: o, theirs: t } => {
                        if o != Input::None {
                            pending_ours = o;
                        }
                        if t != Input::None {
                            pending_theirs = t;
                        }
                    }
                    Command::None => {}
                }
            }
        }

        let now = Instant::now();
        leftover += now.saturating_duration_since(last);
        last = now;
        while leftover >= tick_len && fight.result.is_none() {
            leftover -= tick_len;
            let (oi, ti) = match mode {
                Mode::TwoPlayer => (pending_ours, pending_theirs),
                Mode::VsCpu { human: Side::Ours } => (pending_ours, fight.cpu_input(Side::Theirs)),
                Mode::VsCpu {
                    human: Side::Theirs,
                } => (fight.cpu_input(Side::Ours), pending_ours),
            };
            pending_ours = Input::None;
            pending_theirs = Input::None;
            fight.step(oi, ti);
        }

        draw_fight(path, round, total, ours, theirs, &fight)?;
        if let Some(result) = fight.result {
            draw_ko(result)?;
            wait_key_or(Duration::from_millis(1400))?;
            let pick = match result {
                RoundResult::Ours => Pick::Ours,
                RoundResult::Theirs => Pick::Theirs,
                RoundResult::Draw => return Ok(RoundChoice::Skip),
            };
            return Ok(RoundChoice::Winner(pick));
        }
        if leftover < tick_len {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

enum Command {
    Quit,
    Skip,
    Input { ours: Input, theirs: Input },
    None,
}

fn key_command(key: KeyEvent) -> Command {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Command::Quit;
    }
    match key.code {
        KeyCode::Char('q') => Command::Quit,
        KeyCode::Tab => Command::Skip,
        KeyCode::Char('a') => Command::Input {
            ours: Input::Punch,
            theirs: Input::None,
        },
        KeyCode::Char('s') => Command::Input {
            ours: Input::Kick,
            theirs: Input::None,
        },
        KeyCode::Char('d') => Command::Input {
            ours: Input::Block,
            theirs: Input::None,
        },
        KeyCode::Char('f') => Command::Input {
            ours: Input::Special,
            theirs: Input::None,
        },
        KeyCode::Char('j') => Command::Input {
            ours: Input::None,
            theirs: Input::Punch,
        },
        KeyCode::Char('k') => Command::Input {
            ours: Input::None,
            theirs: Input::Kick,
        },
        KeyCode::Char('l') => Command::Input {
            ours: Input::None,
            theirs: Input::Block,
        },
        KeyCode::Char(';') => Command::Input {
            ours: Input::None,
            theirs: Input::Special,
        },
        _ => Command::None,
    }
}

fn path_seed(path: &Path) -> u64 {
    let mut h = 0x811c_9dc5_u64;
    for b in path.to_string_lossy().as_bytes() {
        h = h.wrapping_mul(0x0100_0193) ^ u64::from(*b);
    }
    h
}

fn select_mode() -> Result<Option<Mode>, String> {
    loop {
        let mut out = io::stdout();
        queue!(
            out,
            Clear(ClearType::All),
            MoveTo(0, 0),
            SetBackgroundColor(BG),
            SetForegroundColor(OURS),
            Print("GIT FIGHT\r\n\r\n"),
            Print("1  vs CPU\r\n"),
            Print("2  two players on one keyboard\r\n"),
            Print("q  quit\r\n"),
            ResetColor
        )
        .map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        match wait_key_or(Duration::from_secs(3600))? {
            Some(KeyCode::Char('1')) => return pick_cpu_side(),
            Some(KeyCode::Char('2')) => return Ok(Some(Mode::TwoPlayer)),
            Some(KeyCode::Char('q')) | None => return Ok(None),
            _ => {}
        }
    }
}

fn pick_cpu_side() -> Result<Option<Mode>, String> {
    loop {
        let mut out = io::stdout();
        queue!(
            out,
            Clear(ClearType::All),
            MoveTo(0, 0),
            SetBackgroundColor(BG),
            SetForegroundColor(OURS),
            Print("play as\r\n\r\n"),
            Print("1  ours (left)\r\n"),
            Print("2  theirs (right)\r\n"),
            Print("q  quit\r\n"),
            ResetColor
        )
        .map_err(|e| e.to_string())?;
        out.flush().map_err(|e| e.to_string())?;
        match wait_key_or(Duration::from_secs(3600))? {
            Some(KeyCode::Char('1')) => return Ok(Some(Mode::VsCpu { human: Side::Ours })),
            Some(KeyCode::Char('2')) => {
                return Ok(Some(Mode::VsCpu {
                    human: Side::Theirs,
                }))
            }
            Some(KeyCode::Char('q')) | None => return Ok(None),
            _ => {}
        }
    }
}

fn wait_key_or(max: Duration) -> Result<Option<KeyCode>, String> {
    if event::poll(max).map_err(|e| e.to_string())? {
        if let Event::Key(k) = event::read().map_err(|e| e.to_string())? {
            if k.kind == event::KeyEventKind::Press {
                return Ok(Some(k.code));
            }
        }
    }
    Ok(None)
}

fn draw_fight(
    path: &Path,
    round: usize,
    total: usize,
    ours: &NamedFighter,
    theirs: &NamedFighter,
    fight: &FightState,
) -> Result<(), String> {
    let mut out = io::stdout();
    queue!(
        out,
        Clear(ClearType::All),
        SetBackgroundColor(BG),
        MoveTo(0, 0),
        SetForegroundColor(OURS),
        Print("GIT FIGHT"),
        MoveTo(40, 0),
        Print(format!("round {}/{total}", round + 1)),
        MoveTo(58, 0),
        Print(format_timer(fight)),
        MoveTo(0, 1),
        Print(trunc(&path.display().to_string(), 70)),
        MoveTo(0, 3),
        SetForegroundColor(OURS),
        Print(format!("{:<16}", trunc(&ours.name, 16))),
        Print(' '),
        Print(hp_bar(fight.ours.hp, fight.ours.max_hp, 16)),
        Print(format!(" {:>3}   {:<3} ", fight.ours.hp, fight.theirs.hp)),
        SetForegroundColor(THEIRS),
        Print(hp_bar(fight.theirs.hp, fight.theirs.max_hp, 16)),
        Print(' '),
        Print(format!("{:>16}", trunc(&theirs.name, 16))),
    )
    .map_err(|e| e.to_string())?;

    draw_sprites(&mut out, fight)?;

    queue!(
        out,
        MoveTo(0, 12),
        SetForegroundColor(OURS),
        Print("a punch  s kick  d block  f special"),
        MoveTo(0, 13),
        SetForegroundColor(THEIRS),
        Print("j punch  k kick  l block  ; special"),
        MoveTo(0, 14),
        SetForegroundColor(OURS),
        Print("tab skip   q quit"),
        ResetColor
    )
    .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn draw_sprites(out: &mut impl Write, fight: &FightState) -> Result<(), String> {
    let ours_pose = fight.ours.pose();
    let theirs_pose = fight.theirs.pose();
    let left = sprite(Side::Ours, ours_pose);
    let right = sprite(Side::Theirs, theirs_pose);
    let max_x = ARENA_W.saturating_sub(SPRITE_COLS as i32).max(0);
    let lx = fight.ours.x.clamp(0, max_x) as u16;
    let rx = fight.theirs.x.clamp(0, max_x) as u16;
    for row in 0..SPRITE_ROWS {
        queue!(
            out,
            MoveTo(lx, 5 + row as u16),
            SetForegroundColor(OURS),
            Print(left[row]),
            MoveTo(rx, 5 + row as u16),
            SetForegroundColor(THEIRS),
            Print(right[row]),
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn draw_ko(result: RoundResult) -> Result<(), String> {
    let (msg, color) = match result {
        RoundResult::Ours => ("KO — OURS", OURS),
        RoundResult::Theirs => ("KO — THEIRS", THEIRS),
        RoundResult::Draw => ("DRAW", OURS),
    };
    let mut out = io::stdout();
    queue!(
        out,
        MoveTo(28, 11),
        SetBackgroundColor(BG),
        SetForegroundColor(color),
        Print(msg),
        ResetColor
    )
    .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn format_timer(fight: &FightState) -> String {
    let left = fight.round_ticks.saturating_sub(fight.tick);
    let secs = left / TICKS_PER_SECOND;
    format!("{secs:>2}")
}

fn hp_bar(hp: i32, max: i32, width: usize) -> String {
    if max <= 0 {
        return " ".repeat(width);
    }
    let filled = ((hp.max(0) as usize) * width) / (max as usize);
    let filled = filled.min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn trunc(s: &str, n: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if out.chars().count() >= n {
            break;
        }
        out.push(c);
    }
    out
}

fn stdout_is_tty() -> bool {
    crossterm::tty::IsTty::is_tty(&io::stdout())
}

pub fn pick_hunks(file: &ConflictFile, path: &Path) -> Result<Option<Vec<Option<Pick>>>, String> {
    if !stdout_is_tty() {
        return Err("need a terminal, or pass --pick".into());
    }
    let _raw = RawGuard::enter()?;
    let n = file.hunk_count();
    let mut picks: Vec<Option<Pick>> = vec![None; n];
    let mut i = 0usize;
    let mut undo: Vec<usize> = Vec::new();
    while i < n {
        draw_pick(file, path, i, n, picks[i])?;
        match wait_key_or(Duration::from_secs(3600))? {
            Some(KeyCode::Char('1')) => {
                picks[i] = Some(Pick::Ours);
                undo.push(i);
                i += 1;
            }
            Some(KeyCode::Char('2')) => {
                picks[i] = Some(Pick::Theirs);
                undo.push(i);
                i += 1;
            }
            Some(KeyCode::Char('b')) => {
                picks[i] = Some(Pick::Both);
                undo.push(i);
                i += 1;
            }
            Some(KeyCode::Char('s')) => {
                picks[i] = None;
                undo.push(i);
                i += 1;
            }
            Some(KeyCode::Char('u')) => {
                if let Some(prev) = undo.pop() {
                    picks[prev] = None;
                    i = prev;
                }
            }
            Some(KeyCode::Char('q')) => return Ok(None),
            None => return Ok(None),
            _ => {}
        }
    }
    Ok(Some(picks))
}

fn draw_pick(
    file: &ConflictFile,
    path: &Path,
    idx: usize,
    total: usize,
    current: Option<Pick>,
) -> Result<(), String> {
    let mut out = io::stdout();
    queue!(
        out,
        Clear(ClearType::All),
        MoveTo(0, 0),
        SetBackgroundColor(BG),
        SetForegroundColor(OURS),
        Print(format!(
            "git fight --no-fight  {}  hunk {}/{total}\r\n",
            path.display(),
            idx + 1
        )),
        Print("1 ours   2 theirs   b both   s skip   u undo   q quit\r\n\r\n"),
        SetForegroundColor(OURS),
        Print("OURS\r\n"),
        Print(preview(file.ours(idx))),
        Print("\r\n"),
        SetForegroundColor(THEIRS),
        Print("THEIRS\r\n"),
        Print(preview(file.theirs(idx))),
        Print("\r\n"),
        SetForegroundColor(OURS),
        Print(match current {
            None => "pending",
            Some(Pick::Ours) => "picked ours",
            Some(Pick::Theirs) => "picked theirs",
            Some(Pick::Both) => "picked both",
        }),
        ResetColor
    )
    .map_err(|e| e.to_string())?;
    out.flush().map_err(|e| e.to_string())
}

fn preview(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.lines()
        .take(8)
        .map(|l| {
            let mut t = trunc(l, 72);
            t.push_str("\r\n");
            t
        })
        .collect()
}
