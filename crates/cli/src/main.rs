mod git;
mod stats;
mod ui;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use git_fight_core::{ConflictFile, ParseError, Pick};

#[derive(Parser, Debug)]
#[command(
    name = "git-fight",
    about = "Resolve merge conflicts by beating up the other branch.",
    version
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Command>,
    /// Side-by-side picker instead of a fight.
    #[arg(long)]
    no_fight: bool,
    /// Resolve every conflict without a UI.
    #[arg(long, value_enum)]
    pick: Option<PickArg>,
    /// Mergetool paths: BASE LOCAL REMOTE MERGED, or none to find conflicted files.
    #[arg(value_name = "PATH")]
    paths: Vec<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print mergetool git config, or write it with --apply.
    Install {
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum PickArg {
    Ours,
    Theirs,
    Both,
}

impl From<PickArg> for Pick {
    fn from(value: PickArg) -> Self {
        match value {
            PickArg::Ours => Pick::Ours,
            PickArg::Theirs => Pick::Theirs,
            PickArg::Both => Pick::Both,
        }
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            let _ = writeln!(io::stderr(), "git fight: {err}");
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<ExitCode, String> {
    let args = Args::parse();
    if let Some(Command::Install { apply }) = args.cmd {
        return git::install(apply).map(|_| ExitCode::SUCCESS);
    }

    if args.paths.len() == 4 {
        let merged = &args.paths[3];
        let local = &args.paths[1];
        let remote = &args.paths[2];
        return handle_file(merged, Some(local), Some(remote), &args);
    }
    if !args.paths.is_empty() {
        return Err("expected no paths, or BASE LOCAL REMOTE MERGED".into());
    }

    let files = git::conflicted_files()?;
    if files.is_empty() {
        println!("no conflicted files");
        return Ok(ExitCode::SUCCESS);
    }
    let mut all_ok = true;
    for file in &files {
        match handle_file(file, None, None, &args)? {
            code if code == ExitCode::SUCCESS => {}
            _ => all_ok = false,
        }
    }
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

fn handle_file(
    merged: &Path,
    local: Option<&Path>,
    remote: Option<&Path>,
    args: &Args,
) -> Result<ExitCode, String> {
    let bytes = std::fs::read(merged).map_err(|e| format!("{}: {e}", merged.display()))?;
    let parsed = match ConflictFile::parse(&bytes) {
        Ok(p) => p,
        Err(ParseError::Binary) => {
            return Err(format!("{}: binary file, left untouched", merged.display()));
        }
        Err(ParseError::Broken(msg)) => {
            return Err(format!(
                "{}: broken conflict markers ({msg}), left untouched",
                merged.display()
            ));
        }
    };

    if parsed.hunk_count() == 0 {
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(pick) = args.pick {
        write_atomically(merged, &parsed.resolve_all(pick.into()))?;
        return Ok(ExitCode::SUCCESS);
    }

    let picks = if args.no_fight {
        ui::pick_hunks(&parsed, merged)?
    } else {
        let (ours, theirs) = stats::fighters(merged, local, remote);
        ui::fight_hunks(&parsed, merged, ours, theirs)?
    };

    match picks {
        None => Ok(ExitCode::from(1)),
        Some(picks) => {
            let resolved = parsed.resolve(&picks);
            write_atomically(merged, &resolved)?;
            let leftover = ConflictFile::parse(&resolved)
                .map(|f| f.hunk_count())
                .unwrap_or(1);
            if leftover == 0 {
                Ok(ExitCode::SUCCESS)
            } else {
                Ok(ExitCode::from(1))
            }
        }
    }
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = path.with_extension("git-fight-tmp");
    {
        let mut f =
            std::fs::File::create(&tmp).map_err(|e| format!("temp {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .map_err(|e| format!("temp {}: {e}", tmp.display()))?;
        f.sync_all().ok();
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("replace {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_file(bytes: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("git-fight-cli-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = dir.join(format!("c{n}.txt"));
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn pick_ours_writes_and_clears() {
        let path = temp_file(b"<<<<<<< a\nOURS\n=======\nTHEIRS\n>>>>>>> b\n");
        let args = Args {
            cmd: None,
            no_fight: false,
            pick: Some(PickArg::Ours),
            paths: vec![
                PathBuf::from("b"),
                PathBuf::from("l"),
                PathBuf::from("r"),
                path.clone(),
            ],
        };
        let code = handle_file(&path, None, None, &args).unwrap();
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(fs::read(&path).unwrap(), b"OURS\n");
    }

    #[test]
    fn pick_both() {
        let path = temp_file(b"<<<<<<< a\nA\n=======\nB\n>>>>>>> b\n");
        let args = Args {
            cmd: None,
            no_fight: false,
            pick: Some(PickArg::Both),
            paths: vec![],
        };
        handle_file(&path, None, None, &args).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"A\nB\n");
    }

    #[test]
    fn binary_refused() {
        let path = temp_file(b"ok\0no");
        let args = Args {
            cmd: None,
            no_fight: false,
            pick: Some(PickArg::Ours),
            paths: vec![],
        };
        let err = handle_file(&path, None, None, &args).unwrap_err();
        assert!(err.contains("binary"));
        assert_eq!(fs::read(&path).unwrap(), b"ok\0no");
    }

    #[test]
    fn broken_refused() {
        let path = temp_file(b"<<<<<<< a\nOURS\n=======\nTHEIRS\n");
        let args = Args {
            cmd: None,
            no_fight: false,
            pick: Some(PickArg::Ours),
            paths: vec![],
        };
        let err = handle_file(&path, None, None, &args).unwrap_err();
        assert!(err.contains("broken"));
        assert_eq!(
            fs::read(&path).unwrap(),
            b"<<<<<<< a\nOURS\n=======\nTHEIRS\n"
        );
    }
}
