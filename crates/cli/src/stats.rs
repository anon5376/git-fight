use std::collections::BTreeSet;
use std::path::Path;

use git_fight_core::FighterStats;

use crate::git;

#[derive(Clone, Debug)]
pub struct NamedFighter {
    pub name: String,
    pub stats: FighterStats,
}

impl NamedFighter {
    fn fallback(label: &str) -> Self {
        Self {
            name: label.to_string(),
            stats: FighterStats::default(),
        }
    }
}

pub fn fighters(
    merged: &Path,
    local: Option<&Path>,
    remote: Option<&Path>,
) -> (NamedFighter, NamedFighter) {
    let path = display_path(merged);
    let ours_name = author_for("HEAD", path)
        .or_else(|| local.and_then(author_of_file))
        .unwrap_or_else(|| "ours".into());
    let other_ref = other_head();
    let theirs_name = other_ref
        .as_deref()
        .and_then(|r| author_for(r, path))
        .or_else(|| remote.and_then(author_of_file))
        .unwrap_or_else(|| "theirs".into());

    let ours = build(&ours_name, "HEAD", path);
    let theirs = match other_ref {
        Some(r) => build(&theirs_name, &r, path),
        None => NamedFighter {
            name: theirs_name,
            stats: FighterStats::default(),
        },
    };
    (ours, theirs)
}

fn display_path(merged: &Path) -> &Path {
    merged
}

fn other_head() -> Option<String> {
    for name in [
        "MERGE_HEAD",
        "REBASE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
    ] {
        if git::git_stdout_ok(&["rev-parse", "-q", "--verify", name]).is_some() {
            return Some(name.to_string());
        }
    }
    None
}

fn author_for(rev: &str, path: &Path) -> Option<String> {
    git::git_stdout_ok(&[
        "log",
        "-1",
        "--format=%an",
        rev,
        "--",
        &path.to_string_lossy(),
    ])
    .filter(|s| !s.is_empty())
}

fn author_of_file(_path: &Path) -> Option<String> {
    None
}

fn build(name: &str, rev: &str, path: &Path) -> NamedFighter {
    let hp = hp_from_blame(name, rev, path);
    let armor = armor_from_commit(rev, path);
    let special = special_from_log(name);
    NamedFighter {
        name: name.to_string(),
        stats: FighterStats::clamped(hp, armor, special),
    }
}

fn hp_from_blame(name: &str, rev: &str, path: &Path) -> i32 {
    let Some(out) = git::git_stdout_ok(&[
        "blame",
        "--line-porcelain",
        rev,
        "--",
        &path.to_string_lossy(),
    ]) else {
        return 100;
    };
    let mut mine = 0i32;
    let mut total = 0i32;
    for line in out.lines() {
        if let Some(author) = line.strip_prefix("author ") {
            total += 1;
            if author == name {
                mine += 1;
            }
        }
    }
    if total <= 0 {
        return 100;
    }
    80 + (mine * 40) / total
}

fn armor_from_commit(rev: &str, path: &Path) -> bool {
    let Some(commit) = git::git_stdout_ok(&[
        "log",
        "-1",
        "--format=%H",
        rev,
        "--",
        &path.to_string_lossy(),
    ]) else {
        return false;
    };
    let Some(files) = git::git_stdout_ok(&["show", "--name-only", "--pretty=format:", &commit])
    else {
        return false;
    };
    files.lines().any(looks_like_test)
}

fn looks_like_test(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.contains("test") || lower.contains("spec")
}

fn special_from_log(name: &str) -> bool {
    let Some(out) = git::git_stdout_ok(&[
        "log",
        "--since=7 days ago",
        "--format=%ad",
        "--date=short",
        &format!("--author={name}"),
    ]) else {
        return false;
    };
    let mut days = BTreeSet::new();
    for line in out.lines() {
        if !line.is_empty() {
            days.insert(line.to_string());
        }
    }
    days.len() >= 3
}

impl Default for NamedFighter {
    fn default() -> Self {
        Self::fallback("fighter")
    }
}
