use chrono::{Duration, Utc};
use git_fight_core::FighterStats;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{Pool, Sqlite, SqlitePool};
use std::str::FromStr;

#[derive(Clone, Debug)]
pub struct MatchRow {
    pub id: String,
    pub seed: String,
    pub status: String,
    pub ours_name: String,
    pub theirs_name: String,
    pub ours_kind: String,
    pub theirs_kind: String,
    pub ours_token: Option<String>,
    pub theirs_token: Option<String>,
    pub ours_login: Option<String>,
    pub theirs_login: Option<String>,
    pub owner: String,
    pub repo: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    pub pr_base_sha: String,
    pub installation_id: Option<i64>,
    pub input_delay_ticks: i64,
    pub created_at: String,
    pub expires_at: String,
    pub final_hash: Option<String>,
    pub abort_reason: Option<String>,
    pub result_branch: Option<String>,
    pub challenge_comment_id: Option<i64>,
}

pub async fn connect(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(opts)
        .await?;
    init_schema(&pool).await?;
    Ok(pool)
}

async fn init_schema(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS matches (
            id TEXT PRIMARY KEY,
            installation_id INTEGER,
            owner TEXT NOT NULL DEFAULT '',
            repo TEXT NOT NULL DEFAULT '',
            pr_number INTEGER NOT NULL DEFAULT 0,
            pr_head_sha TEXT NOT NULL DEFAULT '',
            pr_base_sha TEXT NOT NULL DEFAULT '',
            seed TEXT NOT NULL,
            status TEXT NOT NULL,
            ours_login TEXT,
            theirs_login TEXT,
            ours_name TEXT NOT NULL,
            theirs_name TEXT NOT NULL,
            ours_kind TEXT NOT NULL,
            theirs_kind TEXT NOT NULL,
            ours_token TEXT,
            theirs_token TEXT,
            input_delay_ticks INTEGER NOT NULL DEFAULT 3,
            created_at TEXT NOT NULL,
            started_at TEXT,
            finished_at TEXT,
            expires_at TEXT NOT NULL,
            result_branch TEXT,
            final_hash TEXT,
            abort_reason TEXT,
            challenge_comment_id INTEGER
        )",
    )
    .execute(pool)
    .await?;
    let _ = sqlx::query("ALTER TABLE matches ADD COLUMN challenge_comment_id INTEGER")
        .execute(pool)
        .await;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS matches_one_open_per_pr
         ON matches(owner, repo, pr_number)
         WHERE status IN ('pending', 'in_progress') AND pr_number > 0 AND owner != ''",
    )
    .execute(pool)
    .await?;
    ensure_match_inputs(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS match_hunks (
            match_id TEXT NOT NULL,
            round_index INTEGER NOT NULL,
            path TEXT NOT NULL,
            hunk_index INTEGER NOT NULL,
            ours_bytes BLOB,
            theirs_bytes BLOB,
            base_bytes BLOB,
            theirs_login TEXT,
            theirs_name TEXT,
            winner TEXT,
            ours_hp INTEGER NOT NULL DEFAULT 100,
            ours_armor INTEGER NOT NULL DEFAULT 0,
            ours_special INTEGER NOT NULL DEFAULT 0,
            theirs_hp INTEGER NOT NULL DEFAULT 100,
            theirs_armor INTEGER NOT NULL DEFAULT 0,
            theirs_special INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (match_id, round_index),
            FOREIGN KEY (match_id) REFERENCES matches(id)
        )",
    )
    .execute(pool)
    .await?;
    for (col, ty) in [
        ("ours_hp", "INTEGER NOT NULL DEFAULT 100"),
        ("ours_armor", "INTEGER NOT NULL DEFAULT 0"),
        ("ours_special", "INTEGER NOT NULL DEFAULT 0"),
        ("theirs_hp", "INTEGER NOT NULL DEFAULT 100"),
        ("theirs_armor", "INTEGER NOT NULL DEFAULT 0"),
        ("theirs_special", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        let q = format!("ALTER TABLE match_hunks ADD COLUMN {col} {ty}");
        let _ = sqlx::query(&q).execute(pool).await;
    }
    ensure_match_hunks_fk(pool).await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sessions (
            id TEXT PRIMARY KEY,
            github_user_id INTEGER NOT NULL,
            github_login TEXT NOT NULL,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS webhook_deliveries (
            delivery_id TEXT PRIMARY KEY,
            received_at TEXT NOT NULL,
            body_hash TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await?;
    let _ = sqlx::query("ALTER TABLE webhook_deliveries ADD COLUMN body_hash TEXT")
        .execute(pool)
        .await;
    sqlx::query(
        "CREATE UNIQUE INDEX IF NOT EXISTS webhook_deliveries_body_hash
         ON webhook_deliveries(body_hash)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS player_stats (
            owner TEXT NOT NULL,
            repo TEXT NOT NULL,
            github_login TEXT NOT NULL,
            wins INTEGER NOT NULL DEFAULT 0,
            losses INTEGER NOT NULL DEFAULT 0,
            kos INTEGER NOT NULL DEFAULT 0,
            conflicts_caused INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (owner, repo, github_login)
        )",
    )
    .execute(pool)
    .await?;
    Ok(())
}

async fn ensure_match_inputs(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    let exists: Option<(String,)> = sqlx::query_as(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'match_inputs'",
    )
    .fetch_optional(pool)
    .await?;
    if exists.is_none() {
        sqlx::query(MATCH_INPUTS_DDL).execute(pool).await?;
        return Ok(());
    }
    let cols: Vec<(String,)> = sqlx::query_as("SELECT name FROM pragma_table_info('match_inputs')")
        .fetch_all(pool)
        .await?;
    if !cols.iter().any(|(n,)| n == "round_index") {
        sqlx::query(
            "CREATE TABLE match_inputs_v2 (
                match_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                tick INTEGER NOT NULL,
                ours INTEGER NOT NULL,
                theirs INTEGER NOT NULL,
                PRIMARY KEY (match_id, round_index, tick),
                FOREIGN KEY (match_id) REFERENCES matches(id)
            )",
        )
        .execute(pool)
        .await?;
        sqlx::query(
            "INSERT INTO match_inputs_v2 (match_id, round_index, tick, ours, theirs)
             SELECT match_id, 0, tick, ours, theirs FROM match_inputs",
        )
        .execute(pool)
        .await?;
        sqlx::query("DROP TABLE match_inputs").execute(pool).await?;
        sqlx::query("ALTER TABLE match_inputs_v2 RENAME TO match_inputs")
            .execute(pool)
            .await?;
        return Ok(());
    }
    ensure_match_inputs_fk(pool).await
}

const MATCH_INPUTS_DDL: &str = "CREATE TABLE match_inputs (
                match_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                tick INTEGER NOT NULL,
                ours INTEGER NOT NULL,
                theirs INTEGER NOT NULL,
                PRIMARY KEY (match_id, round_index, tick),
                FOREIGN KEY (match_id) REFERENCES matches(id)
            )";

async fn fk_count(pool: &Pool<Sqlite>, table: &'static str) -> Result<i64, sqlx::Error> {
    let sql = match table {
        "match_hunks" => "SELECT COUNT(*) FROM pragma_foreign_key_list('match_hunks')",
        "match_inputs" => "SELECT COUNT(*) FROM pragma_foreign_key_list('match_inputs')",
        _ => unreachable!("known schema table"),
    };
    let (n,): (i64,) = sqlx::query_as(sql).fetch_one(pool).await?;
    Ok(n)
}

async fn ensure_match_inputs_fk(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    if fk_count(pool, "match_inputs").await? > 0 {
        return Ok(());
    }
    sqlx::query(
        "CREATE TABLE match_inputs_fk (
                match_id TEXT NOT NULL,
                round_index INTEGER NOT NULL,
                tick INTEGER NOT NULL,
                ours INTEGER NOT NULL,
                theirs INTEGER NOT NULL,
                PRIMARY KEY (match_id, round_index, tick),
                FOREIGN KEY (match_id) REFERENCES matches(id)
            )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO match_inputs_fk (match_id, round_index, tick, ours, theirs)
         SELECT match_id, round_index, tick, ours, theirs FROM match_inputs",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TABLE match_inputs").execute(pool).await?;
    sqlx::query("ALTER TABLE match_inputs_fk RENAME TO match_inputs")
        .execute(pool)
        .await?;
    Ok(())
}

async fn ensure_match_hunks_fk(pool: &Pool<Sqlite>) -> Result<(), sqlx::Error> {
    if fk_count(pool, "match_hunks").await? > 0 {
        return Ok(());
    }
    sqlx::query(
        "CREATE TABLE match_hunks_fk (
            match_id TEXT NOT NULL,
            round_index INTEGER NOT NULL,
            path TEXT NOT NULL,
            hunk_index INTEGER NOT NULL,
            ours_bytes BLOB,
            theirs_bytes BLOB,
            base_bytes BLOB,
            theirs_login TEXT,
            theirs_name TEXT,
            winner TEXT,
            ours_hp INTEGER NOT NULL DEFAULT 100,
            ours_armor INTEGER NOT NULL DEFAULT 0,
            ours_special INTEGER NOT NULL DEFAULT 0,
            theirs_hp INTEGER NOT NULL DEFAULT 100,
            theirs_armor INTEGER NOT NULL DEFAULT 0,
            theirs_special INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (match_id, round_index),
            FOREIGN KEY (match_id) REFERENCES matches(id)
        )",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "INSERT INTO match_hunks_fk (
            match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
            theirs_login, theirs_name, winner,
            ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
         )
         SELECT match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
            theirs_login, theirs_name, winner,
            ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
         FROM match_hunks",
    )
    .execute(pool)
    .await?;
    sqlx::query("DROP TABLE match_hunks").execute(pool).await?;
    sqlx::query("ALTER TABLE match_hunks_fk RENAME TO match_hunks")
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_match(
    pool: &SqlitePool,
    id: &str,
    seed: u64,
    delay: u32,
    ours_token: &str,
    theirs_token: &str,
    expire_secs: i64,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let expires = now + Duration::seconds(expire_secs);
    sqlx::query(
        "INSERT INTO matches (
            id, seed, status, ours_name, theirs_name, ours_kind, theirs_kind,
            ours_token, theirs_token, input_delay_ticks, created_at, expires_at
        ) VALUES (?, ?, 'pending', 'ours', 'theirs', 'github', 'github', ?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(seed.to_string())
    .bind(ours_token)
    .bind(theirs_token)
    .bind(i64::from(delay))
    .bind(now.to_rfc3339())
    .bind(expires.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

const MATCH_COLS: &str = "id, seed, status, ours_name, theirs_name, ours_kind, theirs_kind,
                ours_token, theirs_token, ours_login, theirs_login, owner, repo, pr_number,
                pr_head_sha, pr_base_sha, installation_id,
                input_delay_ticks, created_at, expires_at,
                final_hash, abort_reason, result_branch, challenge_comment_id";

pub async fn get_match(pool: &SqlitePool, id: &str) -> Result<Option<MatchRow>, sqlx::Error> {
    sqlx::query_as::<_, MatchRow>(&format!("SELECT {MATCH_COLS} FROM matches WHERE id = ?"))
        .bind(id)
        .fetch_optional(pool)
        .await
}

pub async fn list_live_matches(pool: &SqlitePool) -> Result<Vec<MatchRow>, sqlx::Error> {
    sqlx::query_as::<_, MatchRow>(&format!(
        "SELECT {MATCH_COLS} FROM matches
         WHERE status IN ('pending', 'in_progress')
         AND (pr_number = 0 OR EXISTS (
             SELECT 1 FROM match_hunks WHERE match_hunks.match_id = matches.id
         ))"
    ))
    .fetch_all(pool)
    .await
}

pub async fn load_inputs(
    pool: &SqlitePool,
    id: &str,
    round: u32,
) -> Result<Vec<(u32, u8, u8)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT tick, ours, theirs FROM match_inputs
         WHERE match_id = ? AND round_index = ? ORDER BY tick",
    )
    .bind(id)
    .bind(i64::from(round))
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(t, o, th)| (t as u32, o as u8, th as u8))
        .collect())
}

pub async fn load_all_inputs(
    pool: &SqlitePool,
    id: &str,
) -> Result<Vec<(u32, u32, u8, u8)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, i64, i64, i64)>(
        "SELECT round_index, tick, ours, theirs FROM match_inputs
         WHERE match_id = ? ORDER BY round_index, tick",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(r, t, o, th)| (r as u32, t as u32, o as u8, th as u8))
        .collect())
}

pub async fn clear_inputs(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM match_inputs WHERE match_id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_input(
    pool: &SqlitePool,
    id: &str,
    round: u32,
    tick: u32,
    ours: u8,
    theirs: u8,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR IGNORE INTO match_inputs (match_id, round_index, tick, ours, theirs)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(i64::from(round))
    .bind(i64::from(tick))
    .bind(i64::from(ours))
    .bind(i64::from(theirs))
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_status(
    pool: &SqlitePool,
    id: &str,
    status: &str,
    started: bool,
    finished: bool,
    hash: Option<&str>,
    abort: Option<&str>,
) -> Result<(), sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE matches SET status = ?,
            started_at = CASE WHEN ? THEN COALESCE(started_at, ?) ELSE started_at END,
            finished_at = CASE WHEN ? THEN ? ELSE finished_at END,
            final_hash = COALESCE(?, final_hash),
            abort_reason = COALESCE(?, abort_reason)
         WHERE id = ?",
    )
    .bind(status)
    .bind(if started { 1i64 } else { 0 })
    .bind(&now)
    .bind(if finished { 1i64 } else { 0 })
    .bind(&now)
    .bind(hash)
    .bind(abort)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Abort a still-open match so a rematch `/fight` can start. No-op if finished.
pub async fn abort_open_match(
    pool: &SqlitePool,
    id: &str,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE matches SET status = 'aborted',
            finished_at = ?,
            abort_reason = COALESCE(?, abort_reason)
         WHERE id = ? AND status IN ('pending', 'in_progress')",
    )
    .bind(&now)
    .bind(reason)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Mark a match finished only if it is still open (not aborted/expired).
pub async fn finish_open_match(
    pool: &SqlitePool,
    id: &str,
    hash: &str,
) -> Result<bool, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE matches SET status = 'finished',
            started_at = COALESCE(started_at, ?),
            finished_at = ?,
            final_hash = COALESCE(?, final_hash)
         WHERE id = ? AND status IN ('pending', 'in_progress')",
    )
    .bind(&now)
    .bind(&now)
    .bind(hash)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn expire_pending(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM matches
         WHERE status IN ('pending', 'in_progress') AND expires_at <= ?",
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;
    let ids: Vec<String> = rows.into_iter().map(|r| r.0).collect();
    if !ids.is_empty() {
        sqlx::query(
            "UPDATE matches SET status = 'expired', abort_reason = 'expired', finished_at = ?
             WHERE status IN ('pending', 'in_progress') AND expires_at <= ?",
        )
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    }
    Ok(ids)
}

/// Expire a still-open match. No-op if finished/aborted/already expired.
pub async fn expire_open_match(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE matches SET status = 'expired',
            finished_at = ?,
            abort_reason = COALESCE(?, abort_reason)
         WHERE id = ? AND status IN ('pending', 'in_progress')",
    )
    .bind(&now)
    .bind("expired")
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub struct NewMatch {
    pub id: String,
    pub seed: u64,
    pub delay: u32,
    pub ours_name: String,
    pub theirs_name: String,
    pub ours_kind: String,
    pub theirs_kind: String,
    pub ours_login: Option<String>,
    pub theirs_login: Option<String>,
    pub ours_token: String,
    pub theirs_token: String,
    pub expire_secs: i64,
    pub installation_id: Option<i64>,
    pub owner: String,
    pub repo: String,
    pub pr_number: i64,
    pub pr_head_sha: String,
    pub pr_base_sha: String,
}

pub async fn insert_full_match(pool: &SqlitePool, m: &NewMatch) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let expires = now + Duration::seconds(m.expire_secs);
    sqlx::query(
        "INSERT INTO matches (
            id, installation_id, owner, repo, pr_number, pr_head_sha, pr_base_sha,
            seed, status, ours_login, theirs_login, ours_name, theirs_name,
            ours_kind, theirs_kind, ours_token, theirs_token, input_delay_ticks,
            created_at, expires_at
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&m.id)
    .bind(m.installation_id)
    .bind(&m.owner)
    .bind(&m.repo)
    .bind(m.pr_number)
    .bind(&m.pr_head_sha)
    .bind(&m.pr_base_sha)
    .bind(m.seed.to_string())
    .bind(&m.ours_login)
    .bind(&m.theirs_login)
    .bind(&m.ours_name)
    .bind(&m.theirs_name)
    .bind(&m.ours_kind)
    .bind(&m.theirs_kind)
    .bind(&m.ours_token)
    .bind(&m.theirs_token)
    .bind(i64::from(m.delay))
    .bind(now.to_rfc3339())
    .bind(expires.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub fn is_unique_violation(err: &sqlx::Error) -> bool {
    match err {
        sqlx::Error::Database(db) => db.is_unique_violation(),
        _ => false,
    }
}

pub async fn update_match_fighters(
    pool: &SqlitePool,
    id: &str,
    ours_kind: &str,
    theirs_kind: &str,
    theirs_name: &str,
    theirs_login: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE matches SET ours_kind = ?, theirs_kind = ?, theirs_name = ?, theirs_login = ?
         WHERE id = ?",
    )
    .bind(ours_kind)
    .bind(theirs_kind)
    .bind(theirs_name)
    .bind(theirs_login)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub struct NewHunk<'a> {
    pub match_id: &'a str,
    pub round: i64,
    pub path: &'a str,
    pub hunk_index: i64,
    pub ours: &'a [u8],
    pub theirs: &'a [u8],
    pub base: &'a [u8],
    pub theirs_login: Option<&'a str>,
    pub theirs_name: Option<&'a str>,
    pub ours_stats: FighterStats,
    pub theirs_stats: FighterStats,
}

pub async fn insert_hunk(pool: &SqlitePool, h: &NewHunk<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO match_hunks (
            match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
            theirs_login, theirs_name,
            ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(h.match_id)
    .bind(h.round)
    .bind(h.path)
    .bind(h.hunk_index)
    .bind(h.ours)
    .bind(h.theirs)
    .bind(h.base)
    .bind(h.theirs_login)
    .bind(h.theirs_name)
    .bind(i64::from(h.ours_stats.hp))
    .bind(h.ours_stats.armor as i64)
    .bind(h.ours_stats.special as i64)
    .bind(i64::from(h.theirs_stats.hp))
    .bind(h.theirs_stats.armor as i64)
    .bind(h.theirs_stats.special as i64)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn open_match_for_pr(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<Option<MatchRow>, sqlx::Error> {
    sqlx::query_as::<_, MatchRow>(&format!(
        "SELECT {MATCH_COLS} FROM matches WHERE owner = ? AND repo = ? AND pr_number = ?
         AND status IN ('pending', 'in_progress') LIMIT 1"
    ))
    .bind(owner)
    .bind(repo)
    .bind(pr as i64)
    .fetch_optional(pool)
    .await
}

pub async fn count_recent_matches_for_install(
    pool: &SqlitePool,
    installation_id: u64,
    within_secs: i64,
) -> Result<i64, sqlx::Error> {
    let cutoff = (Utc::now() - Duration::seconds(within_secs)).to_rfc3339();
    sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM matches WHERE installation_id = ? AND created_at >= ?",
    )
    .bind(installation_id as i64)
    .bind(cutoff)
    .fetch_one(pool)
    .await
    .map(|r| r.0)
}

pub async fn count_recent_matches_for_pr(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    pr: u64,
    within_secs: i64,
) -> Result<i64, sqlx::Error> {
    let cutoff = (Utc::now() - Duration::seconds(within_secs)).to_rfc3339();
    sqlx::query_as::<_, (i64,)>(
        "SELECT COUNT(*) FROM matches WHERE owner = ? AND repo = ? AND pr_number = ? AND created_at >= ?",
    )
    .bind(owner)
    .bind(repo)
    .bind(pr as i64)
    .bind(cutoff)
    .fetch_one(pool)
    .await
    .map(|r| r.0)
}

pub fn theirs_login_for_round<'a>(
    hunks: &'a [HunkRow],
    round: u32,
    fallback: Option<&'a str>,
) -> Option<&'a str> {
    hunks
        .iter()
        .find(|h| h.round_index == i64::from(round))
        .and_then(|h| h.theirs_login.as_deref())
        .or(fallback)
}

pub fn theirs_name_for_round(hunks: &[HunkRow], round: u32, fallback: &str) -> String {
    hunks
        .iter()
        .find(|h| h.round_index == i64::from(round))
        .and_then(|h| h.theirs_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

pub fn github_identity(row: &MatchRow, hunks: &[HunkRow]) -> bool {
    row.ours_login.is_some()
        || row.theirs_login.is_some()
        || hunks.iter().any(|h| h.theirs_login.is_some())
}

pub async fn record_delivery(
    pool: &SqlitePool,
    id: &str,
    body_hash: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, received_at, body_hash)
         VALUES (?, ?, ?)",
    )
    .bind(id)
    .bind(Utc::now().to_rfc3339())
    .bind(body_hash)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn prune_deliveries(pool: &SqlitePool, max_age_secs: i64) -> Result<u64, sqlx::Error> {
    let cutoff = (Utc::now() - Duration::seconds(max_age_secs)).to_rfc3339();
    let res = sqlx::query("DELETE FROM webhook_deliveries WHERE received_at <= ?")
        .bind(cutoff)
        .execute(pool)
        .await?;
    Ok(res.rows_affected())
}

pub async fn insert_session(
    pool: &SqlitePool,
    id: &str,
    user_id: i64,
    login: &str,
) -> Result<(), sqlx::Error> {
    let now = Utc::now();
    let expires = now + Duration::days(14);
    sqlx::query(
        "INSERT INTO sessions (id, github_user_id, github_login, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(id)
    .bind(user_id)
    .bind(login)
    .bind(now.to_rfc3339())
    .bind(expires.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn session_login(pool: &SqlitePool, id: &str) -> Result<Option<String>, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    sqlx::query_as::<_, (String,)>(
        "SELECT github_login FROM sessions WHERE id = ? AND expires_at > ?",
    )
    .bind(id)
    .bind(now)
    .fetch_optional(pool)
    .await
    .map(|r| r.map(|x| x.0))
}

pub async fn set_hunk_winner(
    pool: &SqlitePool,
    match_id: &str,
    round: i64,
    winner: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE match_hunks SET winner = ? WHERE match_id = ? AND round_index = ?")
        .bind(winner)
        .bind(match_id)
        .bind(round)
        .execute(pool)
        .await?;
    Ok(())
}

pub struct HunkRow {
    pub round_index: i64,
    pub path: String,
    pub hunk_index: i64,
    pub winner: Option<String>,
    pub theirs_name: Option<String>,
    pub theirs_login: Option<String>,
    pub ours_hp: i32,
    pub ours_armor: bool,
    pub ours_special: bool,
    pub theirs_hp: i32,
    pub theirs_armor: bool,
    pub theirs_special: bool,
}

impl HunkRow {
    pub fn ours_stats(&self) -> FighterStats {
        FighterStats::clamped(self.ours_hp, self.ours_armor, self.ours_special)
    }

    pub fn theirs_stats(&self) -> FighterStats {
        FighterStats::clamped(self.theirs_hp, self.theirs_armor, self.theirs_special)
    }
}

pub fn stats_for_round(hunks: &[HunkRow], round: u32) -> (FighterStats, FighterStats) {
    hunks
        .iter()
        .find(|h| h.round_index == i64::from(round))
        .map(|h| (h.ours_stats(), h.theirs_stats()))
        .unwrap_or_default()
}

pub fn hunk_meta_for_round(hunks: &[HunkRow], round: u32) -> (String, u32) {
    hunks
        .iter()
        .find(|h| h.round_index == i64::from(round))
        .map(|h| (h.path.clone(), h.hunk_index.max(0) as u32))
        .unwrap_or_else(|| (String::new(), 0))
}

pub async fn list_hunks(pool: &SqlitePool, match_id: &str) -> Result<Vec<HunkRow>, sqlx::Error> {
    sqlx::query_as::<_, HunkRow>(
        "SELECT round_index, path, hunk_index, winner, theirs_name, theirs_login,
                ours_hp, ours_armor, ours_special, theirs_hp, theirs_armor, theirs_special
         FROM match_hunks WHERE match_id = ? ORDER BY round_index",
    )
    .bind(match_id)
    .fetch_all(pool)
    .await
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlayerStat {
    pub github_login: String,
    pub wins: i64,
    pub losses: i64,
    pub kos: i64,
    pub conflicts_caused: i64,
}

pub async fn add_player_stats(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    stat: &PlayerStat,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO player_stats (owner, repo, github_login, wins, losses, kos, conflicts_caused)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(owner, repo, github_login) DO UPDATE SET
            wins = wins + excluded.wins,
            losses = losses + excluded.losses,
            kos = kos + excluded.kos,
            conflicts_caused = conflicts_caused + excluded.conflicts_caused",
    )
    .bind(owner)
    .bind(repo)
    .bind(&stat.github_login)
    .bind(stat.wins)
    .bind(stat.losses)
    .bind(stat.kos)
    .bind(stat.conflicts_caused)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn list_player_stats(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
) -> Result<Vec<PlayerStat>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (String, i64, i64, i64, i64)>(
        "SELECT github_login, wins, losses, kos, conflicts_caused
         FROM player_stats
         WHERE owner = ? AND repo = ?
         ORDER BY wins DESC, kos DESC, conflicts_caused DESC, github_login COLLATE NOCASE ASC",
    )
    .bind(owner)
    .bind(repo)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(github_login, wins, losses, kos, conflicts_caused)| PlayerStat {
                github_login,
                wins,
                losses,
                kos,
                conflicts_caused,
            },
        )
        .collect())
}

pub async fn get_player_stats(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    login: &str,
) -> Result<PlayerStat, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, i64, i64, i64)>(
        "SELECT github_login, wins, losses, kos, conflicts_caused
         FROM player_stats
         WHERE owner = ? AND repo = ? AND github_login = ? COLLATE NOCASE",
    )
    .bind(owner)
    .bind(repo)
    .bind(login)
    .fetch_optional(pool)
    .await?;
    Ok(row
        .map(
            |(github_login, wins, losses, kos, conflicts_caused)| PlayerStat {
                github_login,
                wins,
                losses,
                kos,
                conflicts_caused,
            },
        )
        .unwrap_or(PlayerStat {
            github_login: login.to_string(),
            wins: 0,
            losses: 0,
            kos: 0,
            conflicts_caused: 0,
        }))
}

pub async fn set_challenge_comment_id(
    pool: &SqlitePool,
    id: &str,
    comment_id: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE matches SET challenge_comment_id = ? WHERE id = ?")
        .bind(comment_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_result_branch(
    pool: &SqlitePool,
    id: &str,
    branch: Option<&str>,
    abort: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE matches SET result_branch = COALESCE(?, result_branch),
            abort_reason = COALESCE(?, abort_reason)
         WHERE id = ?",
    )
    .bind(branch)
    .bind(abort)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

impl sqlx::FromRow<'_, sqlx::sqlite::SqliteRow> for HunkRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        let flag =
            |name: &str| -> Result<bool, sqlx::Error> { Ok(row.try_get::<i64, _>(name)? != 0) };
        Ok(Self {
            round_index: row.try_get("round_index")?,
            path: row.try_get("path")?,
            hunk_index: row.try_get("hunk_index")?,
            winner: row.try_get("winner")?,
            theirs_name: row.try_get("theirs_name")?,
            theirs_login: row.try_get("theirs_login")?,
            ours_hp: row.try_get::<i64, _>("ours_hp")? as i32,
            ours_armor: flag("ours_armor")?,
            ours_special: flag("ours_special")?,
            theirs_hp: row.try_get::<i64, _>("theirs_hp")? as i32,
            theirs_armor: flag("theirs_armor")?,
            theirs_special: flag("theirs_special")?,
        })
    }
}

impl sqlx::FromRow<'_, sqlx::sqlite::SqliteRow> for MatchRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, sqlx::Error> {
        use sqlx::Row;
        Ok(Self {
            id: row.try_get("id")?,
            seed: row.try_get("seed")?,
            status: row.try_get("status")?,
            ours_name: row.try_get("ours_name")?,
            theirs_name: row.try_get("theirs_name")?,
            ours_kind: row.try_get("ours_kind")?,
            theirs_kind: row.try_get("theirs_kind")?,
            ours_token: row.try_get("ours_token")?,
            theirs_token: row.try_get("theirs_token")?,
            ours_login: row.try_get("ours_login")?,
            theirs_login: row.try_get("theirs_login")?,
            owner: row.try_get("owner")?,
            repo: row.try_get("repo")?,
            pr_number: row.try_get("pr_number")?,
            pr_head_sha: row.try_get("pr_head_sha")?,
            pr_base_sha: row.try_get("pr_base_sha")?,
            installation_id: row.try_get("installation_id")?,
            input_delay_ticks: row.try_get("input_delay_ticks")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
            final_hash: row.try_get("final_hash")?,
            abort_reason: row.try_get("abort_reason")?,
            result_branch: row.try_get("result_branch")?,
            challenge_comment_id: row.try_get("challenge_comment_id")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn expire_pending_leaves_no_hash() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_match(&pool, "abc", 1, 3, "o", "t", 0).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let ids = expire_pending(&pool).await.unwrap();
        assert_eq!(ids, vec!["abc".to_string()]);
        let row = get_match(&pool, "abc").await.unwrap().unwrap();
        assert_eq!(row.status, "expired");
        assert!(row.final_hash.is_none());
        assert_eq!(row.abort_reason.as_deref(), Some("expired"));
    }

    #[tokio::test]
    async fn expire_pending_expires_in_progress_and_skips_finished() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_full_match(
            &pool,
            &NewMatch {
                id: "live1".into(),
                seed: 1,
                delay: 3,
                ours_name: "a".into(),
                theirs_name: "b".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: None,
                theirs_login: None,
                ours_token: "o".into(),
                theirs_token: "t".into(),
                expire_secs: 0,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        set_status(&pool, "live1", "in_progress", true, false, None, None)
            .await
            .unwrap();
        insert_full_match(
            &pool,
            &NewMatch {
                id: "fin1".into(),
                seed: 1,
                delay: 3,
                ours_name: "a".into(),
                theirs_name: "b".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: None,
                theirs_login: None,
                ours_token: "o".into(),
                theirs_token: "t".into(),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 2,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        assert!(finish_open_match(&pool, "fin1", "deadbeef").await.unwrap());
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let ids = expire_pending(&pool).await.unwrap();
        assert_eq!(ids, vec!["live1".to_string()]);
        let live = get_match(&pool, "live1").await.unwrap().unwrap();
        assert_eq!(live.status, "expired");
        assert_eq!(live.abort_reason.as_deref(), Some("expired"));
        assert!(open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .is_none());
        insert_full_match(
            &pool,
            &NewMatch {
                id: "live2".into(),
                seed: 2,
                delay: 3,
                ours_name: "a".into(),
                theirs_name: "b".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: None,
                theirs_login: None,
                ours_token: "o2".into(),
                theirs_token: "t2".into(),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        let fin = get_match(&pool, "fin1").await.unwrap().unwrap();
        assert_eq!(fin.status, "finished");
        assert_eq!(fin.final_hash.as_deref(), Some("deadbeef"));
        assert!(!expire_open_match(&pool, "fin1").await.unwrap());
    }

    #[tokio::test]
    async fn clear_inputs_and_install_rate_count() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_match(&pool, "abc", 1, 3, "o", "t", 60)
            .await
            .unwrap();
        insert_input(&pool, "abc", 0, 0, 1, 2).await.unwrap();
        insert_input(&pool, "abc", 0, 1, 3, 4).await.unwrap();
        insert_input(&pool, "abc", 1, 0, 5, 6).await.unwrap();
        assert_eq!(load_inputs(&pool, "abc", 0).await.unwrap().len(), 2);
        assert_eq!(load_inputs(&pool, "abc", 1).await.unwrap().len(), 1);
        insert_input(&pool, "abc", 0, 0, 9, 9).await.unwrap();
        assert_eq!(
            load_inputs(&pool, "abc", 0).await.unwrap()[0],
            (0, 1, 2),
            "confirmed ticks are append-only"
        );
        assert_eq!(load_all_inputs(&pool, "abc").await.unwrap().len(), 3);
        clear_inputs(&pool, "abc").await.unwrap();
        assert!(load_all_inputs(&pool, "abc").await.unwrap().is_empty());

        let m = NewMatch {
            id: "inst1".into(),
            seed: 1,
            delay: 3,
            ours_name: "a".into(),
            theirs_name: "b".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "o".into(),
            theirs_token: "t".into(),
            expire_secs: 3600,
            installation_id: Some(9),
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: "h".into(),
            pr_base_sha: "b".into(),
        };
        insert_full_match(&pool, &m).await.unwrap();
        assert_eq!(
            count_recent_matches_for_install(&pool, 9, 3600)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            count_recent_matches_for_install(&pool, 8, 3600)
                .await
                .unwrap(),
            0
        );
        let open = open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(open.id, "inst1");
        assert_eq!(open.pr_head_sha, "h");
        assert_eq!(
            count_recent_matches_for_pr(&pool, "acme", "box", 1, 3600)
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            count_recent_matches_for_pr(&pool, "acme", "box", 2, 3600)
                .await
                .unwrap(),
            0
        );
        assert!(open.challenge_comment_id.is_none());
        set_challenge_comment_id(&pool, "inst1", 99).await.unwrap();
        let stored = get_match(&pool, "inst1").await.unwrap().unwrap();
        assert_eq!(stored.challenge_comment_id, Some(99));
    }

    #[tokio::test]
    async fn one_open_match_per_pr_then_abort_frees_the_slot() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let mut m = NewMatch {
            id: "open1".into(),
            seed: 1,
            delay: 3,
            ours_name: "a".into(),
            theirs_name: "b".into(),
            ours_kind: "github".into(),
            theirs_kind: "cpu".into(),
            ours_login: None,
            theirs_login: None,
            ours_token: "o".into(),
            theirs_token: "t".into(),
            expire_secs: 3600,
            installation_id: Some(1),
            owner: "acme".into(),
            repo: "box".into(),
            pr_number: 1,
            pr_head_sha: "h".into(),
            pr_base_sha: "b".into(),
        };
        insert_full_match(&pool, &m).await.unwrap();
        m.id = "open2".into();
        let err = insert_full_match(&pool, &m).await.unwrap_err();
        assert!(is_unique_violation(&err), "{err}");
        set_status(&pool, "open1", "aborted", false, true, None, Some("clone"))
            .await
            .unwrap();
        insert_full_match(&pool, &m).await.unwrap();
        let open = open_match_for_pr(&pool, "acme", "box", 1)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(open.id, "open2");
    }

    #[tokio::test]
    async fn abort_open_match_does_not_clobber_finished() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_full_match(
            &pool,
            &NewMatch {
                id: "fin1".into(),
                seed: 1,
                delay: 3,
                ours_name: "a".into(),
                theirs_name: "b".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: None,
                theirs_login: None,
                ours_token: "o".into(),
                theirs_token: "t".into(),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        assert!(finish_open_match(&pool, "fin1", "deadbeef").await.unwrap());
        assert!(!abort_open_match(&pool, "fin1", "outdated").await.unwrap());
        let row = get_match(&pool, "fin1").await.unwrap().unwrap();
        assert_eq!(row.status, "finished");
        assert_eq!(row.final_hash.as_deref(), Some("deadbeef"));
        assert!(row.abort_reason.is_none());
    }

    #[tokio::test]
    async fn live_rooms_skip_pr_matches_until_hunks_exist() {
        let pool = connect("sqlite::memory:").await.unwrap();
        insert_full_match(
            &pool,
            &NewMatch {
                id: "nohunks".into(),
                seed: 1,
                delay: 3,
                ours_name: "a".into(),
                theirs_name: "b".into(),
                ours_kind: "github".into(),
                theirs_kind: "cpu".into(),
                ours_login: None,
                theirs_login: None,
                ours_token: "o".into(),
                theirs_token: "t".into(),
                expire_secs: 3600,
                installation_id: Some(1),
                owner: "acme".into(),
                repo: "box".into(),
                pr_number: 1,
                pr_head_sha: "h".into(),
                pr_base_sha: "b".into(),
            },
        )
        .await
        .unwrap();
        assert!(list_live_matches(&pool).await.unwrap().is_empty());
        insert_hunk(
            &pool,
            &NewHunk {
                match_id: "nohunks",
                round: 0,
                path: "lib.rs",
                hunk_index: 0,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: None,
                ours_stats: git_fight_core::FighterStats::default(),
                theirs_stats: git_fight_core::FighterStats::default(),
            },
        )
        .await
        .unwrap();
        let live = list_live_matches(&pool).await.unwrap();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, "nohunks");
    }

    fn is_fk(err: &sqlx::Error) -> bool {
        err.as_database_error()
            .map(|e| e.message().to_ascii_uppercase().contains("FOREIGN KEY"))
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn hunks_and_inputs_need_a_match_row() {
        let pool = connect("sqlite::memory:").await.unwrap();
        let err = insert_hunk(
            &pool,
            &NewHunk {
                match_id: "missing",
                round: 0,
                path: "lib.rs",
                hunk_index: 0,
                ours: b"a",
                theirs: b"b",
                base: b"c",
                theirs_login: None,
                theirs_name: None,
                ours_stats: git_fight_core::FighterStats::default(),
                theirs_stats: git_fight_core::FighterStats::default(),
            },
        )
        .await
        .unwrap_err();
        assert!(is_fk(&err), "{err}");
        let err = insert_input(&pool, "missing", 0, 0, 1, 2)
            .await
            .unwrap_err();
        assert!(is_fk(&err), "{err}");
    }

    #[tokio::test]
    async fn delivery_dedup_by_id_and_body_hash() {
        let pool = connect("sqlite::memory:").await.unwrap();
        assert!(record_delivery(&pool, "d1", "hash-a").await.unwrap());
        assert!(!record_delivery(&pool, "d1", "hash-a").await.unwrap());
        assert!(!record_delivery(&pool, "d2", "hash-a").await.unwrap());
        assert!(record_delivery(&pool, "d2", "hash-b").await.unwrap());
        assert!(!record_delivery(&pool, "d3", "hash-b").await.unwrap());
        sqlx::query("UPDATE webhook_deliveries SET received_at = '2000-01-01T00:00:00+00:00'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(prune_deliveries(&pool, 24 * 60 * 60).await.unwrap(), 2);
        assert!(record_delivery(&pool, "d1", "hash-a").await.unwrap());
    }
}
