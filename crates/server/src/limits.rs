//! Limits so one repo cannot hog the server.

use std::time::Duration;

pub const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
/// GitHub `size` is kilobytes. 1 GiB.
pub const MAX_REPO_KB: u64 = 1_048_576;
pub const MAX_HUNKS: usize = 15;
pub const MAX_BLOB_BYTES: usize = 1_048_576;
/// New `/fight` (and auto-challenge) starts per GitHub installation per hour.
pub const MAX_MATCHES_PER_INSTALL_HOUR: i64 = 20;
/// New `/fight` (and auto-challenge) starts per pull request per hour.
pub const MAX_MATCHES_PER_PR_HOUR: i64 = 5;
/// Clone / merge-tree / push jobs at once (webhook returns 200 before git work).
/// HTTP (mergeable poll, comments) must not take a slot.
pub const MAX_CONCURRENT_GIT: usize = 2;

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
        assert_eq!(MAX_BLOB_BYTES, 1_048_576);
        assert_eq!(MAX_CONCURRENT_GIT, 2);
        assert_eq!(MAX_MATCHES_PER_PR_HOUR, 5);
        assert_eq!(MAX_MATCHES_PER_INSTALL_HOUR, 20);
    }
}
