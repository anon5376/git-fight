//! Limits so one repo cannot hog the server.

use std::time::Duration;

pub const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
/// GitHub `size` is kilobytes. 1 GiB.
pub const MAX_REPO_KB: u64 = 1_048_576;
pub const MAX_HUNKS: usize = 15;
/// Conflicted paths we will inspect. More than this is TooMany without cat-file.
pub const MAX_CONFLICT_PATHS: usize = 32;
pub const MAX_BLOB_BYTES: usize = 1_048_576;
/// Wall clock while a git worker slot is held (clone already caps at 60s).
pub const GIT_JOB_TIMEOUT: Duration = Duration::from_secs(120);
/// New `/fight` (and auto-challenge) starts per GitHub installation per hour.
pub const MAX_MATCHES_PER_INSTALL_HOUR: i64 = 20;
/// New `/fight` (and auto-challenge) starts per pull request per hour.
pub const MAX_MATCHES_PER_PR_HOUR: i64 = 5;
/// Clone / merge-tree / push jobs at once (webhook returns 200 before git work).
/// HTTP (mergeable poll, install token, comments) must not take a slot.
/// A slot is held at most GIT_JOB_TIMEOUT.
pub const MAX_CONCURRENT_GIT: usize = 2;
/// `.github/git-fight.yml` is one key. Bigger is not a config file.
pub const MAX_FIGHT_YML_BYTES: usize = 4096;
/// Drop webhook deliveries (and reject GitHub event timestamps) older than a match.
pub const WEBHOOK_MAX_AGE_SECS: i64 = 24 * 60 * 60;
/// Signed session cookie and `sessions.expires_at`. Expired rows are pruned.
pub const SESSION_TTL_SECS: i64 = 14 * 24 * 60 * 60;

pub fn git_slots() -> &'static tokio::sync::Semaphore {
    static SLOTS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    SLOTS.get_or_init(|| tokio::sync::Semaphore::new(MAX_CONCURRENT_GIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hard_rule_caps() {
        assert_eq!(CLONE_TIMEOUT, Duration::from_secs(60));
        assert_eq!(MAX_REPO_KB, 1_048_576);
        assert_eq!(MAX_HUNKS, 15);
        assert_eq!(MAX_CONFLICT_PATHS, 32);
        assert_eq!(MAX_BLOB_BYTES, 1_048_576);
        assert_eq!(GIT_JOB_TIMEOUT, Duration::from_secs(120));
        assert_eq!(MAX_CONCURRENT_GIT, 2);
        assert_eq!(MAX_MATCHES_PER_PR_HOUR, 5);
        assert_eq!(MAX_MATCHES_PER_INSTALL_HOUR, 20);
        assert_eq!(MAX_FIGHT_YML_BYTES, 4096);
        assert_eq!(WEBHOOK_MAX_AGE_SECS, 24 * 60 * 60);
        assert_eq!(WEBHOOK_MAX_AGE_SECS, crate::protocol::EXPIRE_SECS);
        assert_eq!(SESSION_TTL_SECS, 14 * 24 * 60 * 60);
    }
}
