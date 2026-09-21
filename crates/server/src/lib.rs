mod app;
mod auth;
mod challenge;
pub mod db;
pub mod gh;
pub mod gitutil;
mod limits;
pub mod protocol;
pub mod result;
mod room;
pub mod sig;
mod stats;
mod webhook;

pub use app::{router, serve, AppState, Config};
pub use auth::{sign as sign_session, Auth};
pub use db::connect as db_connect;
pub use gh::GitHub;
pub use limits::{MAX_MATCHES_PER_INSTALL_HOUR, MAX_MATCHES_PER_PR_HOUR};
pub use protocol::EXPIRE_SECS as protocol_expire_secs;
pub use protocol::INPUT_DELAY;
pub use result::{publish as publish_result, ResultCtx};
pub use stats::record_round;

/// Unique directory for integration-test SQLite files.
/// `pid` + wall-clock nanos collides when two tokio tests start in the same
/// tick; they then share a DB and trip `matches_one_open_per_pr`.
pub fn test_tmp_dir(prefix: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "{prefix}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("test tmp dir");
    dir
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_tmp_dirs_do_not_collide() {
        let a = crate::test_tmp_dir("gf-uniq");
        let b = crate::test_tmp_dir("gf-uniq");
        assert_ne!(a, b);
        assert!(a.starts_with(std::env::temp_dir()));
        assert!(b.starts_with(std::env::temp_dir()));
    }
}
