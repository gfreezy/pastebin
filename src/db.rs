use sqlx::{Row, SqlitePool};

use crate::error::AppError;
use crate::models::*;

pub async fn init_db(db: &SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pastes (
            id TEXT PRIMARY KEY,
            title TEXT,
            content TEXT NOT NULL,
            visibility TEXT NOT NULL,
            access_key TEXT,
            created_at INTEGER NOT NULL,
            expires_at INTEGER
        );
        "#,
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_pastes_expires_at ON pastes(expires_at);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_pastes_created_at ON pastes(created_at);",
    )
    .execute(db)
    .await?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
    )
    .execute(db)
    .await?;

    Ok(())
}

pub async fn get_setting(db: &SqlitePool, key: &str) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await?;
    Ok(row.map(|r| r.get("value")))
}

pub async fn set_setting(db: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES (?, ?)")
        .bind(key)
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn delete_setting(db: &SqlitePool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM settings WHERE key = ?")
        .bind(key)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn purge_expired(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let now = now_ts();
    sqlx::query(
        "DELETE FROM pastes WHERE expires_at IS NOT NULL AND expires_at <= ?",
    )
    .bind(now)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn fetch_paste_meta(state: &AppState) -> Result<Vec<PasteMeta>, AppError> {
    let now = now_ts();
    let rows = sqlx::query(
        r#"
        SELECT id, title, visibility, access_key, created_at, expires_at
        FROM pastes
        WHERE expires_at IS NULL OR expires_at > ?
        ORDER BY created_at DESC
        "#,
    )
    .bind(now)
    .fetch_all(&state.db)
    .await?;

    let mut items = Vec::with_capacity(rows.len());
    for row in rows {
        let id: String = row.get("id");
        let visibility = parse_visibility_string(row.get::<String, _>("visibility"))?;
        let access_key = row.get::<Option<String>, _>("access_key");
        let raw_url = build_raw_url(&state.base_url, &id, visibility, access_key.as_deref());
        items.push(PasteMeta {
            id,
            title: row.get::<Option<String>, _>("title"),
            visibility: visibility.as_str().to_string(),
            created_at: format_ts(row.get::<i64, _>("created_at")),
            expires_at: row.get::<Option<i64>, _>("expires_at").map(format_ts),
            raw_url,
            needs_key: visibility == Visibility::Private,
        });
    }
    Ok(items)
}
