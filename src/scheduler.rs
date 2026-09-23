//! Durable cron state lives alongside the paste. A lease prevents duplicate
//! execution across scheduler ticks (and multiple processes sharing SQLite).
use crate::{
    error::AppError,
    models::{AppState, now_ts},
};
use chrono::TimeZone;
use chrono_tz::Tz;
use croner::Cron;
use sqlx::Row;
use std::sync::Arc;

pub fn normalize(
    value: Option<&str>,
    kind: &str,
    timezone: Tz,
) -> Result<Option<String>, AppError> {
    let Some(pattern) = value.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    if kind != "javascript" {
        return Err(AppError::BadRequest(
            "scheduler is only available for JavaScript pastes".into(),
        ));
    }
    if pattern.split_whitespace().count() != 5 {
        return Err(AppError::BadRequest(
            "scheduler must have five fields: minute hour day month weekday".into(),
        ));
    }
    next_run(pattern, timezone, now_ts()).map_err(AppError::BadRequest)?;
    Ok(Some(pattern.to_string()))
}

pub fn next_run(pattern: &str, timezone: Tz, after: i64) -> Result<i64, String> {
    let cron: Cron = pattern
        .parse()
        .map_err(|e| format!("Invalid scheduler: {e}"))?;
    let after = timezone
        .timestamp_opt(after, 0)
        .single()
        .ok_or("Invalid schedule timestamp")?;
    cron.find_next_occurrence(&after, false)
        .map(|next| next.timestamp())
        .map_err(|e| format!("No next schedule occurrence: {e}"))
}

pub async fn run(state: Arc<AppState>) {
    let mut ticks = tokio::time::interval(std::time::Duration::from_secs(1));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut jobs = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = ticks.tick() => {
                while jobs.len() < 2 {
                    match claim(&state).await {
                        Ok(Some(job)) => { let state = state.clone(); jobs.spawn(async move { execute_job(state, job).await; }); }
                        Ok(None) => break,
                        Err(error) => { tracing::error!("scheduler claim failed: {error}"); break; }
                    }
                }
            }
            Some(result) = jobs.join_next(), if !jobs.is_empty() => {
                if let Err(error) = result { tracing::error!("scheduler task failed: {error}"); }
            }
        }
    }
}

struct Job {
    id: String,
    source: String,
    pattern: String,
    revision: i64,
    token: String,
}

async fn claim(state: &AppState) -> Result<Option<Job>, sqlx::Error> {
    let now = now_ts();
    let token = nanoid::nanoid!();
    let row = sqlx::query(
        "UPDATE pastes SET run_token = ?, running_until = ? WHERE id = (
            SELECT id FROM pastes WHERE kind = 'javascript' AND scheduler IS NOT NULL
            AND next_run_at <= ? AND (running_until IS NULL OR running_until <= ?)
            AND (expires_at IS NULL OR expires_at > ?) ORDER BY next_run_at, id LIMIT 1
        ) RETURNING id, content, scheduler, revision",
    )
    .bind(&token)
    .bind(now + 120)
    .bind(now)
    .bind(now)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;
    Ok(row.map(|row| Job {
        id: row.get("id"),
        source: row.get("content"),
        pattern: row.get("scheduler"),
        revision: row.get("revision"),
        token,
    }))
}

async fn execute_job(state: Arc<AppState>, job: Job) {
    // Wait for a slot rather than interpreting interactive preview load as a
    // script failure. The executor's run deadline starts once the slot is held.
    let result = state.scripts.execute_scheduled(job.source).await;
    let now = now_ts();
    let next = next_run(&job.pattern, state.script_timezone, now);
    let next_at = next.as_ref().ok().copied();
    let error = result.error.or_else(|| next.err());
    let output = result.output;
    let query = sqlx::query(
        "UPDATE pastes SET cached_content = CASE WHEN ? IS NOT NULL THEN ? ELSE cached_content END,
        cache_updated_at = CASE WHEN ? IS NOT NULL THEN ? ELSE cache_updated_at END,
        last_run_at = ?, last_error = ?, next_run_at = ?, running_until = NULL, run_token = NULL
        WHERE id = ? AND revision = ? AND run_token = ? AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(output.as_deref())
    .bind(output.as_deref())
    .bind(output.as_deref())
    .bind(now)
    .bind(now)
    .bind(error)
    .bind(next_at)
    .bind(&job.id)
    .bind(job.revision)
    .bind(&job.token)
    .bind(now)
    .execute(&state.db)
    .await;
    if let Err(error) = query {
        tracing::error!(
            paste_id = job.id,
            "scheduler result persistence failed: {error}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    #[test]
    fn cron_validation_and_timezone() {
        let tz = chrono_tz::Asia::Shanghai;
        let after = Utc
            .with_ymd_and_hms(2026, 9, 23, 0, 0, 0)
            .unwrap()
            .timestamp();
        let expected = Utc
            .with_ymd_and_hms(2026, 9, 23, 1, 0, 0)
            .unwrap()
            .timestamp();
        assert_eq!(next_run("0 9 * * *", tz, after).unwrap(), expected);
        assert!(normalize(Some("* * * * * *"), "javascript", tz).is_err());
        assert!(normalize(Some("61 * * * *"), "javascript", tz).is_err());
        assert!(normalize(Some("* * * * *"), "text", tz).is_err());
        assert_eq!(normalize(Some("  "), "javascript", tz).unwrap(), None);
    }
    async fn response_text(response: axum::response::Response) -> String {
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn durable_cache_migration_and_edit_races() {
        use crate::{db, handlers, models::*, script::ScriptExecutor};
        use axum::{
            Json,
            extract::{Form, Path, Query, State},
            http::StatusCode,
            response::IntoResponse,
        };
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        // Exercise upgrade from the existing schema, not just a fresh install.
        sqlx::query("CREATE TABLE pastes (id TEXT PRIMARY KEY, title TEXT, content TEXT NOT NULL, visibility TEXT NOT NULL, access_key TEXT, created_at INTEGER NOT NULL, expires_at INTEGER)").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO pastes (id,content,visibility,created_at) VALUES ('old','legacy','public',0)").execute(&db).await.unwrap();
        db::init_db(&db).await.unwrap();
        db::init_db(&db).await.unwrap();
        let state = Arc::new(AppState {
            db,
            scripts: ScriptExecutor::from_env().unwrap(),
            script_timezone: chrono_tz::Asia::Shanghai,
            admin_user: "test".into(),
            admin_pass: "test".into(),
            base_url: "http://localhost".into(),
            sessions: Default::default(),
        });
        let raw = |id: &str, key: Option<&str>| {
            handlers::raw_paste(
                Path(id.to_owned()),
                Query(RawQuery {
                    key: key.map(str::to_owned),
                }),
                State(state.clone()),
            )
        };
        assert_eq!(
            response_text(raw("old", None).await.unwrap()).await,
            "legacy"
        );
        let form: CreatePasteForm = serde_json::from_value(serde_json::json!({
            "id":"scheduled", "content":"export default () => 'cached';", "kind":"javascript", "scheduler":"*/5 * * * *",
            "visibility":"private", "access_password":"secret"
        })).unwrap();
        handlers::create_paste(State(state.clone()), Form(form))
            .await
            .unwrap();
        assert_eq!(
            raw("scheduled", None).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            raw("scheduled", Some("secret")).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        let job = claim(&state).await.unwrap().unwrap();
        assert!(claim(&state).await.unwrap().is_none());
        execute_job(state.clone(), job).await;
        assert_eq!(
            response_text(raw("scheduled", Some("secret")).await.unwrap()).await,
            "cached"
        );
        assert!(claim(&state).await.unwrap().is_none());
        sqlx::query("UPDATE pastes SET next_run_at = 0 WHERE id = 'scheduled'")
            .execute(&state.db)
            .await
            .unwrap();
        let mut job = claim(&state).await.unwrap().unwrap();
        job.source = "throw new Error('upstream unavailable');".into();
        execute_job(state.clone(), job).await;
        assert_eq!(
            response_text(raw("scheduled", Some("secret")).await.unwrap()).await,
            "cached"
        );
        let error: String =
            sqlx::query_scalar("SELECT last_error FROM pastes WHERE id = 'scheduled'")
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert!(error.contains("upstream unavailable"));
        // An in-flight old revision must not overwrite a freshly edited paste.
        sqlx::query("UPDATE pastes SET next_run_at = 0 WHERE id = 'scheduled'")
            .execute(&state.db)
            .await
            .unwrap();
        let stale = claim(&state).await.unwrap().unwrap();
        let update =
            serde_json::from_value(serde_json::json!({"content":"export default () => 'new';"})).unwrap();
        handlers::update_paste(Path("scheduled".into()), State(state.clone()), Json(update))
            .await
            .unwrap();
        execute_job(state.clone(), stale).await;
        assert_eq!(
            raw("scheduled", Some("secret")).await.unwrap().status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        execute_job(state.clone(), claim(&state).await.unwrap().unwrap()).await;
        assert_eq!(
            response_text(raw("scheduled", Some("secret")).await.unwrap()).await,
            "new"
        );
        // Empty scheduler switches to run-on-request and invalidates the cache.
        let update =
            serde_json::from_value(serde_json::json!({"scheduler":"", "content":"export default () => 'live';"}))
                .unwrap();
        handlers::update_paste(Path("scheduled".into()), State(state.clone()), Json(update))
            .await
            .unwrap();
        assert!(claim(&state).await.unwrap().is_none());
        assert_eq!(
            response_text(raw("scheduled", Some("secret")).await.unwrap()).await,
            "live"
        );
        let invalid =
            serde_json::from_value(serde_json::json!({"scheduler":"not a cron"})).unwrap();
        assert!(
            handlers::update_paste(
                Path("scheduled".into()),
                State(state.clone()),
                Json(invalid)
            )
            .await
            .is_err()
        );
        // Text conversion clears scheduling and returns literal content.
        let update = serde_json::from_value(serde_json::json!({"kind":"text"})).unwrap();
        handlers::update_paste(Path("scheduled".into()), State(state.clone()), Json(update))
            .await
            .unwrap();
        assert_eq!(
            response_text(raw("scheduled", Some("secret")).await.unwrap()).await,
            "export default () => 'live';"
        );
        let view = handlers::view_paste(Path("scheduled".into()), State(state.clone()))
            .await
            .unwrap()
            .into_response();
        assert!(response_text(view).await.contains("Run preview"));
        sqlx::query("UPDATE pastes SET expires_at = 1 WHERE id = 'scheduled'")
            .execute(&state.db)
            .await
            .unwrap();
        assert!(raw("scheduled", Some("secret")).await.is_err());
    }
}
