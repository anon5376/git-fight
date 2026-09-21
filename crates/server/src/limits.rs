//! Limits so one repo cannot hog the server.

use std::time::Duration;

pub const CLONE_TIMEOUT: Duration = Duration::from_secs(60);
/// GitHub `size` is kilobytes. 1 GiB.
pub const MAX_REPO_KB: u64 = 1_048_576;
pub const MAX_HUNKS: usize = 15;
pub const MAX_BLOB_BYTES: usize = 1_048_576;
