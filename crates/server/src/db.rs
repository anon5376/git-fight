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
            PRIMARY KEY (match_id, round_index)
        )",
    )
    .execute(pool)
    .await?;
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
            received_at TEXT NOT NULL
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
                ours_token, theirs_token, ours_login, theirs_login, owner, repo, pr_number,
                pr_head_sha, pr_base_sha, installation_id,
                input_delay_ticks, created_at, expires_at,
                final_hash, abort_reason, result_branch
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
}

pub async fn insert_hunk(pool: &SqlitePool, h: &NewHunk<'_>) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO match_hunks (
            match_id, round_index, path, hunk_index, ours_bytes, theirs_bytes, base_bytes,
            theirs_login, theirs_name
        ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
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
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn open_match_for_pr(
    pool: &SqlitePool,
    owner: &str,
    repo: &str,
    pr: u64,
) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_as::<_, (String,)>(
        "SELECT id FROM matches WHERE owner = ? AND repo = ? AND pr_number = ?
         AND status IN ('pending', 'in_progress') LIMIT 1",
    )
    .bind(owner)
    .bind(repo)
    .bind(pr as i64)
    .fetch_optional(pool)
    .await
    .map(|r| r.map(|x| x.0))
}

pub async fn record_delivery(pool: &SqlitePool, id: &str) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO webhook_deliveries (delivery_id, received_at) VALUES (?, ?)",
    )
    .bind(id)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
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
}

pub async fn list_hunks(pool: &SqlitePool, match_id: &str) -> Result<Vec<HunkRow>, sqlx::Error> {
    let rows = sqlx::query_as::<_, (i64, String, i64, Option<String>, Option<String>)>(
        "SELECT round_index, path, hunk_index, winner, theirs_name
         FROM match_hunks WHERE match_id = ? ORDER BY round_index",
    )
    .bind(match_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(round_index, path, hunk_index, winner, theirs_name)| HunkRow {
                round_index,
                path,
                hunk_index,
                winner,
                theirs_name,
            },
        )
        .collect())
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
