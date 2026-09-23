use askama::Template;
use axum::{
    extract::{Form, Path, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use sqlx::Row;
use std::sync::Arc;

use crate::db::{fetch_paste_meta, purge_expired};
use crate::error::AppError;
use crate::models::*;

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    pastes: Vec<PasteMeta>,
    script_timezone: String,
}

#[derive(Template)]
#[template(path = "view.html")]
struct ViewTemplate {
    paste: PasteDetail,
    script_timezone: String,
    editing: bool,
}

#[derive(Template)]
#[template(path = "new.html")]
struct NewTemplate {
    script_timezone: String,
}

pub async fn new_paste(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    NewTemplate {
        script_timezone: state.script_timezone.to_string(),
    }
}

pub async fn edit_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let axum::Json(paste) = get_paste(Path(id), State(state.clone())).await?;
    Ok(ViewTemplate {
        paste,
        script_timezone: state.script_timezone.to_string(),
        editing: true,
    })
}

pub async fn admin_index(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    purge_expired(&state.db).await?;
    let pastes = fetch_paste_meta(&state).await?;
    Ok(IndexTemplate {
        pastes,
        script_timezone: state.script_timezone.to_string(),
    })
}

pub async fn list_pastes(
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<Vec<PasteMeta>>, AppError> {
    purge_expired(&state.db).await?;
    let pastes = fetch_paste_meta(&state).await?;
    Ok(axum::Json(pastes))
}

pub async fn get_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<axum::Json<PasteDetail>, AppError> {
    let now = now_ts();
    let row = sqlx::query(
        r#"
        SELECT id, title, kind, scheduler, cached_content, cache_updated_at, last_run_at, last_error, next_run_at, content, visibility, access_key, created_at, expires_at
        FROM pastes
        WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)
        "#,
    )
    .bind(&id)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound);
    };

    let visibility = parse_visibility_string(row.get::<String, _>("visibility"))?;
    let access_key = row.get::<Option<String>, _>("access_key");
    let raw_url = build_raw_url(&state.base_url, &id, visibility, access_key.as_deref());

    let detail = PasteDetail {
        kind: row.get("kind"),
        scheduler: row.get("scheduler"),
        cached_content: row.get("cached_content"),
        cache_updated_at: row.get::<Option<i64>, _>("cache_updated_at").map(format_ts),
        last_run_at: row.get::<Option<i64>, _>("last_run_at").map(format_ts),
        last_error: row.get("last_error"),
        next_run_at: row.get::<Option<i64>, _>("next_run_at").map(format_ts),
        id,
        title: row.get::<Option<String>, _>("title"),
        visibility: visibility.as_str().to_string(),
        content: row.get::<String, _>("content"),
        created_at: format_ts(row.get::<i64, _>("created_at")),
        expires_at: row.get::<Option<i64>, _>("expires_at").map(format_ts),
        raw_url,
        needs_key: visibility == Visibility::Private,
    };

    Ok(axum::Json(detail))
}

pub async fn create_paste(
    State(state): State<Arc<AppState>>,
    Form(form): Form<CreatePasteForm>,
) -> Result<impl IntoResponse, AppError> {
    purge_expired(&state.db).await?;

    if form.content.trim().is_empty() {
        return Err(AppError::BadRequest("content is required".into()));
    }

    let kind = parse_kind(form.kind.as_deref())?;
    if kind == "javascript" && form.content.len() > crate::script::MAX_SOURCE {
        return Err(AppError::BadRequest(
            "JavaScript source exceeds 256 KiB".into(),
        ));
    }
    let scheduler =
        crate::scheduler::normalize(form.scheduler.as_deref(), kind, state.script_timezone)?;
    let visibility = parse_visibility(form.visibility)?;
    let expires_at = parse_expiry(form.expires_at.as_deref(), form.expires_in)?;

    let access_key = if visibility == Visibility::Private {
        let key = form
            .access_password
            .clone()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| nanoid::nanoid!(16));
        Some(key)
    } else {
        None
    };

    let id = form
        .id
        .clone()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| nanoid::nanoid!(10));
    let created_at = now_ts();

    sqlx::query(
        r#"
        INSERT INTO pastes (id, title, content, visibility, access_key, created_at, expires_at, kind, scheduler, next_run_at)
        VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&id)
    .bind(form.title.as_deref())
    .bind(&form.content)
    .bind(visibility.as_str())
    .bind(access_key.as_deref())
    .bind(created_at)
    .bind(expires_at)
    .bind(kind)
    .bind(scheduler.as_deref())
    .bind(scheduler.as_ref().map(|_| created_at))
    .execute(&state.db)
    .await?;

    let raw_url = build_raw_url(&state.base_url, &id, visibility, access_key.as_deref());

    let response = CreatePasteResponse {
        kind: kind.to_string(),
        scheduler,
        id,
        visibility: visibility.as_str().to_string(),
        created_at: format_ts(created_at),
        expires_at: expires_at.map(format_ts),
        raw_url,
    };

    Ok((StatusCode::CREATED, axum::Json(response)))
}

pub async fn raw_paste(
    Path(id): Path<String>,
    Query(query): Query<RawQuery>,
    State(state): State<Arc<AppState>>,
) -> Result<Response, AppError> {
    let now = now_ts();
    let row = sqlx::query(
        r#"
        SELECT content, kind, scheduler, cached_content, cache_updated_at, visibility, access_key
        FROM pastes
        WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)
        "#,
    )
    .bind(&id)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound);
    };

    let visibility = parse_visibility_string(row.get::<String, _>("visibility"))?;
    if visibility == Visibility::Private {
        let Some(key) = query.key else {
            return Ok((StatusCode::UNAUTHORIZED, "missing key").into_response());
        };
        let stored_key = row.get::<Option<String>, _>("access_key");
        let Some(stored_key) = stored_key else {
            return Ok((StatusCode::UNAUTHORIZED, "missing key").into_response());
        };
        if key != stored_key {
            return Ok((StatusCode::UNAUTHORIZED, "invalid key").into_response());
        }
    }

    let content = row.get::<String, _>("content");
    if row.get::<String, _>("kind") == "javascript" {
        if row.get::<Option<String>, _>("scheduler").is_some() {
            return Ok(match row.get::<Option<String>, _>("cached_content") {
                Some(content) => (
                    StatusCode::OK,
                    [
                        (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-store"),
                        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                    ],
                    content,
                )
                    .into_response(),
                None => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [
                        (header::CACHE_CONTROL, "no-store"),
                        (header::RETRY_AFTER, "5"),
                    ],
                    "Scheduled result is not ready yet",
                )
                    .into_response(),
            });
        }
        let result = state.scripts.execute(content).await;
        return Ok(match result.error {
            Some(_) => (
                StatusCode::BAD_GATEWAY,
                [(header::CACHE_CONTROL, "no-store")],
                "Script execution failed; see administrator preview",
            )
                .into_response(),
            None => (
                StatusCode::OK,
                [
                    (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                    (header::CACHE_CONTROL, "no-store"),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
                ],
                result.output.unwrap_or_default(),
            )
                .into_response(),
        });
    }
    Ok((
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        content,
    )
        .into_response())
}

pub async fn delete_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let result = sqlx::query("DELETE FROM pastes WHERE id = ?")
        .bind(&id)
        .execute(&state.db)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound);
    }

    Ok(StatusCode::NO_CONTENT)
}

pub async fn view_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    let now = now_ts();
    let row = sqlx::query(
        r#"
        SELECT id, title, kind, scheduler, cached_content, cache_updated_at, last_run_at, last_error, next_run_at, content, visibility, access_key, created_at, expires_at
        FROM pastes
        WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)
        "#,
    )
    .bind(&id)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound);
    };

    let visibility = parse_visibility_string(row.get::<String, _>("visibility"))?;
    let access_key = row.get::<Option<String>, _>("access_key");
    let raw_url = build_raw_url(&state.base_url, &id, visibility, access_key.as_deref());

    let paste = PasteDetail {
        kind: row.get("kind"),
        scheduler: row.get("scheduler"),
        cached_content: row.get("cached_content"),
        cache_updated_at: row.get::<Option<i64>, _>("cache_updated_at").map(format_ts),
        last_run_at: row.get::<Option<i64>, _>("last_run_at").map(format_ts),
        last_error: row.get("last_error"),
        next_run_at: row.get::<Option<i64>, _>("next_run_at").map(format_ts),
        id,
        title: row.get::<Option<String>, _>("title"),
        visibility: visibility.as_str().to_string(),
        content: row.get::<String, _>("content"),
        created_at: format_ts(row.get::<i64, _>("created_at")),
        expires_at: row.get::<Option<i64>, _>("expires_at").map(format_ts),
        raw_url,
        needs_key: visibility == Visibility::Private,
    };

    Ok(ViewTemplate {
        paste,
        script_timezone: state.script_timezone.to_string(),
        editing: false,
    })
}

pub async fn update_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    axum::Json(form): axum::Json<UpdatePasteForm>,
) -> Result<impl IntoResponse, AppError> {
    let now = now_ts();
    // Check paste exists and is not expired
    let mut transaction = state.db.begin_with("BEGIN IMMEDIATE").await?;
    let row = sqlx::query(
        "SELECT id, kind, scheduler, content, visibility, access_key, expires_at FROM pastes WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(&id)
    .bind(now)
    .fetch_optional(&mut *transaction)
    .await?;

    let Some(row) = row else {
        return Err(AppError::NotFound);
    };

    let visibility = parse_visibility_string(
        form.visibility
            .clone()
            .unwrap_or_else(|| row.get("visibility")),
    )?;
    let access_key = if visibility == Visibility::Private {
        Some(match form.access_password.as_ref() {
            Some(key) if !key.trim().is_empty() => key.clone(),
            Some(_) => nanoid::nanoid!(16),
            None => row
                .get::<Option<String>, _>("access_key")
                .unwrap_or_else(|| nanoid::nanoid!(16)),
        })
    } else {
        None
    };
    let expires_at = if form.expires_at.is_some() || form.expires_in.is_some() {
        parse_expiry(form.expires_at.as_deref(), form.expires_in)?
    } else {
        row.get::<Option<i64>, _>("expires_at")
    };

    if let Some(ref content) = form.content {
        if content.trim().is_empty() {
            return Err(AppError::BadRequest("content cannot be empty".into()));
        }
    }

    let old_kind: String = row.get("kind");
    let old_scheduler: Option<String> = row.get("scheduler");
    let kind = parse_kind(form.kind.as_deref().or(Some(old_kind.as_str())))?;
    if kind == "javascript"
        && form
            .content
            .as_deref()
            .unwrap_or(&row.get::<String, _>("content"))
            .len()
            > crate::script::MAX_SOURCE
    {
        return Err(AppError::BadRequest(
            "JavaScript source exceeds 256 KiB".into(),
        ));
    }
    let scheduler = if kind == "text" && form.scheduler.as_deref().unwrap_or("").trim().is_empty() {
        None
    } else {
        crate::scheduler::normalize(
            form.scheduler.as_deref().or(old_scheduler.as_deref()),
            kind,
            state.script_timezone,
        )?
    };
    let changed = kind != old_kind
        || scheduler != old_scheduler
        || form
            .content
            .as_ref()
            .is_some_and(|content| content != &row.get::<String, _>("content"));
    sqlx::query("UPDATE pastes SET visibility = ?, access_key = ?, expires_at = ?, title = COALESCE(?, title), content = COALESCE(?, content), kind = ?, scheduler = ?,
        revision = revision + ?, cached_content = CASE WHEN ? THEN NULL ELSE cached_content END,
        cache_updated_at = CASE WHEN ? THEN NULL ELSE cache_updated_at END,
        last_run_at = CASE WHEN ? THEN NULL ELSE last_run_at END,
        last_error = CASE WHEN ? THEN NULL ELSE last_error END,
        next_run_at = CASE WHEN ? THEN ? ELSE next_run_at END,
        running_until = CASE WHEN ? THEN NULL ELSE running_until END,
        run_token = CASE WHEN ? THEN NULL ELSE run_token END WHERE id = ?")
        .bind(visibility.as_str()).bind(access_key).bind(expires_at)
        .bind(form.title.as_deref()).bind(form.content.as_deref()).bind(kind).bind(scheduler.as_deref())
        .bind(i64::from(changed)).bind(changed).bind(changed).bind(changed).bind(changed)
        .bind(changed).bind(scheduler.as_ref().map(|_| now))
        .bind(changed).bind(changed).bind(&id)
        .execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
pub struct PreviewForm {
    content: String,
}

pub async fn preview_script(
    State(state): State<Arc<AppState>>,
    axum::Json(form): axum::Json<PreviewForm>,
) -> impl IntoResponse {
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(state.scripts.execute(form.content).await),
    )
}

#[cfg(test)]
mod access_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn access_key_expiry_and_cache_updates() {
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        crate::db::init_db(&db).await.unwrap();
        let state = Arc::new(AppState {
            db,
            scripts: crate::script::ScriptExecutor::from_env().unwrap(),
            script_timezone: chrono_tz::Asia::Shanghai,
            admin_user: "test".into(),
            admin_pass: "test".into(),
            base_url: "http://localhost".into(),
            sessions: Default::default(),
        });
        create_paste(State(state.clone()), Form(serde_json::from_value(json!({
            "id":"access-test", "content":"export default () => 'cached';", "kind":"javascript", "scheduler":"*/5 * * * *"
        })).unwrap())).await.unwrap();
        sqlx::query("UPDATE pastes SET cached_content = 'cached', cache_updated_at = ? WHERE id = 'access-test'")
            .bind(now_ts()).execute(&state.db).await.unwrap();
        let update = |value| {
            update_paste(
                Path("access-test".into()),
                State(state.clone()),
                axum::Json(serde_json::from_value(value).unwrap()),
            )
        };
        let detail = || get_paste(Path("access-test".into()), State(state.clone()));
        let raw = |key: Option<&str>| {
            raw_paste(
                Path("access-test".into()),
                Query(RawQuery {
                    key: key.map(str::to_owned),
                }),
                State(state.clone()),
            )
        };

        update(json!({"visibility":"private", "access_password":"first & key", "expires_in":3600}))
            .await
            .unwrap();
        let axum::Json(first) = detail().await.unwrap();
        assert!(first.raw_url.contains("key=first%20%26%20key"));
        assert_eq!(first.cached_content.as_deref(), Some("cached"));
        assert!(first.expires_at.is_some());
        assert_eq!(raw(None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            raw(Some("first & key")).await.unwrap().status(),
            StatusCode::OK
        );
        update(json!({"title":"New title"})).await.unwrap();
        let axum::Json(preserved) = detail().await.unwrap();
        assert_eq!(first.raw_url, preserved.raw_url);
        assert_eq!(first.expires_at, preserved.expires_at);

        update(json!({"access_password":"new-key", "expires_at":""}))
            .await
            .unwrap();
        assert_eq!(
            raw(Some("first & key")).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(raw(Some("new-key")).await.unwrap().status(), StatusCode::OK);
        assert!(detail().await.unwrap().0.expires_at.is_none());
        update(json!({"access_password":""})).await.unwrap();
        let generated = detail().await.unwrap().0.raw_url;
        assert!(generated.contains("?key="));
        assert!(!generated.ends_with("?key="));
        assert_eq!(
            raw(Some("new-key")).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );

        for invalid in [
            json!({"visibility":"invalid"}),
            json!({"expires_at":"invalid"}),
            json!({"expires_at":"2000-01-01T00:00:00Z"}),
            json!({"expires_in":-1}),
            json!({"expires_in":i64::MAX}),
            json!({"expires_in":3600,"expires_at":""}),
        ] {
            assert!(matches!(
                update(invalid).await,
                Err(AppError::BadRequest(_))
            ));
        }
        assert_eq!(
            detail().await.unwrap().0.raw_url,
            generated,
            "invalid updates must be atomic"
        );
        let future = format_ts(now_ts() + 7200);
        update(json!({"expires_at":future})).await.unwrap();
        assert_eq!(
            detail().await.unwrap().0.expires_at.as_deref(),
            Some(future.as_str())
        );
        update(json!({"visibility":"public"})).await.unwrap();
        let axum::Json(public) = detail().await.unwrap();
        assert_eq!(public.raw_url, "http://localhost/raw/access-test");
        assert_eq!(public.cached_content.as_deref(), Some("cached"));
        assert_eq!(raw(None).await.unwrap().status(), StatusCode::OK);
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT revision FROM pastes WHERE id = 'access-test'")
                .fetch_one(&state.db)
                .await
                .unwrap(),
            0
        );
        update(json!({"visibility":"private"})).await.unwrap();
        assert_ne!(detail().await.unwrap().0.raw_url, generated);
        assert_eq!(raw(None).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }
}
