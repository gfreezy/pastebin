use sqlx::{Row, SqlitePool};

use crate::error::AppError;
use crate::models::*;

pub async fn init_db(db: &SqlitePool) -> Result<(), sqlx::Error> {
    let mut transaction = db.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS pastes (
            id TEXT PRIMARY KEY,
            title TEXT,
            kind TEXT NOT NULL DEFAULT 'text',
            content TEXT NOT NULL,
            visibility TEXT NOT NULL,
            access_key TEXT,
            created_at INTEGER NOT NULL,
            expires_at INTEGER
        );
        "#,
    )
    .execute(&mut *transaction)
    .await?;

    // Migrate databases created before JavaScript pastes were introduced.
    let columns = sqlx::query("PRAGMA table_info(pastes)")
        .fetch_all(&mut *transaction)
        .await?;
    if !columns
        .iter()
        .any(|row| row.get::<String, _>("name") == "kind")
    {
        sqlx::query("ALTER TABLE pastes ADD COLUMN kind TEXT NOT NULL DEFAULT 'text'")
            .execute(&mut *transaction)
            .await?;
    }

    for (name, definition) in [
        ("scheduler", "TEXT"),
        ("cached_content", "TEXT"),
        ("cache_updated_at", "INTEGER"),
        ("last_run_at", "INTEGER"),
        ("last_error", "TEXT"),
        ("next_run_at", "INTEGER"),
        ("revision", "INTEGER NOT NULL DEFAULT 0"),
        ("running_until", "INTEGER"),
        ("run_token", "TEXT"),
    ] {
        if !columns
            .iter()
            .any(|row| row.get::<String, _>("name") == name)
        {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE pastes ADD COLUMN {name} {definition}"
            )))
            .execute(&mut *transaction)
            .await?;
        }
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_pastes_schedule ON pastes(next_run_at) WHERE scheduler IS NOT NULL")
        .execute(&mut *transaction).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_pastes_expires_at ON pastes(expires_at);")
        .execute(&mut *transaction)
        .await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_pastes_created_at ON pastes(created_at);")
        .execute(&mut *transaction)
        .await?;

    sqlx::query("CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .execute(&mut *transaction)
        .await?;

    transaction.commit().await?;
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
    sqlx::query("DELETE FROM pastes WHERE expires_at IS NOT NULL AND expires_at <= ?")
        .bind(now)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn fetch_paste_meta(state: &AppState) -> Result<Vec<PasteMeta>, AppError> {
    let now = now_ts();
    let rows = sqlx::query(
        r#"
        SELECT id, title, kind, scheduler, visibility, access_key, created_at, expires_at
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
            kind: row.get("kind"),
            scheduler: row.get("scheduler"),
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
