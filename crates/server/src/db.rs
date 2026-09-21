use chrono::{Duration, Utc};
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
    pub input_delay_ticks: i64,
    pub created_at: String,
    pub expires_at: String,
    pub final_hash: Option<String>,
    pub abort_reason: Option<String>,
}

pub async fn connect(url: &str) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(url)?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
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
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS match_inputs (
            match_id TEXT NOT NULL,
            tick INTEGER NOT NULL,
            ours INTEGER NOT NULL,
            theirs INTEGER NOT NULL,
            PRIMARY KEY (match_id, tick)
        )",
    )
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

pub async fn get_match(pool: &SqlitePool, id: &str) -> Result<Option<MatchRow>, sqlx::Error> {
    sqlx::query_as::<_, MatchRow>(
        "SELECT id, seed, status, ours_name, theirs_name, ours_kind, theirs_kind,
                ours_token, theirs_token, input_delay_ticks, created_at, expires_at,
                final_hash, abort_reason
         FROM matches WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
}

pub async fn load_inputs(pool: &SqlitePool, id: &str) -> Result<Vec<(u32, u8, u8)>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, i64, i64)>(
        "SELECT tick, ours, theirs FROM match_inputs WHERE match_id = ? ORDER BY tick",
    )
    .bind(id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(t, o, th)| (t as u32, o as u8, th as u8))
        .collect())
}

pub async fn insert_input(
    pool: &SqlitePool,
    id: &str,
    tick: u32,
    ours: u8,
    theirs: u8,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT OR REPLACE INTO match_inputs (match_id, tick, ours, theirs) VALUES (?, ?, ?, ?)",
    )
    .bind(id)
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

pub async fn expire_pending(pool: &SqlitePool) -> Result<Vec<String>, sqlx::Error> {
    let now = Utc::now().to_rfc3339();
    let rows = sqlx::query_as::<_, (String,)>(
        "SELECT id FROM matches WHERE status = 'pending' AND expires_at <= ?",
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;
    let ids: Vec<String> = rows.into_iter().map(|r| r.0).collect();
    if !ids.is_empty() {
        sqlx::query(
            "UPDATE matches SET status = 'expired', abort_reason = 'expired', finished_at = ?
             WHERE status = 'pending' AND expires_at <= ?",
        )
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
    }
    Ok(ids)
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
            input_delay_ticks: row.try_get("input_delay_ticks")?,
            created_at: row.try_get("created_at")?,
            expires_at: row.try_get("expires_at")?,
            final_hash: row.try_get("final_hash")?,
            abort_reason: row.try_get("abort_reason")?,
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
}
