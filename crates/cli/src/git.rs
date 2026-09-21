use std::path::PathBuf;
use std::process::{Command, Stdio};

pub fn install(apply: bool) -> Result<(), String> {
    let cmd = "git-fight \"$BASE\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\"";
    let lines = mergetool_commands();
    if apply {
        run_git(&["config", "--global", "mergetool.fight.cmd", cmd])?;
        run_git(&[
            "config",
            "--global",
            "mergetool.fight.trustExitCode",
            "true",
        ])?;
        run_git(&["config", "--global", "merge.tool", "fight"])?;
        println!("installed git fight as the default merge tool");
    } else {
        for line in lines {
            println!("{line}");
        }
    }
    Ok(())
}

pub fn conflicted_files() -> Result<Vec<PathBuf>, String> {
    let out = git_stdout(&["diff", "--name-only", "--diff-filter=U"])?;
    let mut files = Vec::new();
    for line in out.split('\n') {
        if !line.is_empty() {
            files.push(PathBuf::from(line));
        }
    }
    Ok(files)
}

pub fn git_stdout(args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git {}: {err}", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub fn git_stdout_ok(args: &[&str]) -> Option<String> {
    git_stdout(args).ok().map(|s| s.trim().to_string())
}

fn run_git(args: &[&str]) -> Result<(), String> {
    git_stdout(args).map(|_| ())
}

pub fn mergetool_commands() -> [String; 3] {
    let cmd = "git-fight \"$BASE\" \"$LOCAL\" \"$REMOTE\" \"$MERGED\"";
    [
        format!("git config --global mergetool.fight.cmd '{cmd}'"),
        "git config --global mergetool.fight.trustExitCode true".to_string(),
        "git config --global merge.tool fight".to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_commands_match_readme() {
        let lines = mergetool_commands();
        assert!(lines[0].contains("git-fight"));
        assert!(lines[1].contains("trustExitCode true"));
        assert!(lines[2].contains("merge.tool fight"));
    }
}
