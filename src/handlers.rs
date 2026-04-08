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
}

#[derive(Template)]
#[template(path = "view.html")]
struct ViewTemplate {
    paste: PasteDetail,
}

pub async fn admin_index(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, AppError> {
    purge_expired(&state.db).await?;
    let pastes = fetch_paste_meta(&state).await?;
    Ok(IndexTemplate { pastes })
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
        SELECT id, title, content, visibility, access_key, created_at, expires_at
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
        INSERT INTO pastes (id, title, content, visibility, access_key, created_at, expires_at)
        VALUES (?, ?, ?, ?, ?, ?, ?)
        "#,
    )
    .bind(&id)
    .bind(form.title.as_deref())
    .bind(&form.content)
    .bind(visibility.as_str())
    .bind(access_key.as_deref())
    .bind(created_at)
    .bind(expires_at)
    .execute(&state.db)
    .await?;

    let raw_url = build_raw_url(&state.base_url, &id, visibility, access_key.as_deref());

    let response = CreatePasteResponse {
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
        SELECT content, visibility, access_key
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
        SELECT id, title, content, visibility, access_key, created_at, expires_at
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
        id,
        title: row.get::<Option<String>, _>("title"),
        visibility: visibility.as_str().to_string(),
        content: row.get::<String, _>("content"),
        created_at: format_ts(row.get::<i64, _>("created_at")),
        expires_at: row.get::<Option<i64>, _>("expires_at").map(format_ts),
        raw_url,
        needs_key: visibility == Visibility::Private,
    };

    Ok(ViewTemplate { paste })
}

pub async fn update_paste(
    Path(id): Path<String>,
    State(state): State<Arc<AppState>>,
    axum::Json(form): axum::Json<UpdatePasteForm>,
) -> Result<impl IntoResponse, AppError> {
    let now = now_ts();
    // Check paste exists and is not expired
    let row = sqlx::query(
        "SELECT id FROM pastes WHERE id = ? AND (expires_at IS NULL OR expires_at > ?)",
    )
    .bind(&id)
    .bind(now)
    .fetch_optional(&state.db)
    .await?;

    if row.is_none() {
        return Err(AppError::NotFound);
    }

    if let Some(ref content) = form.content {
        if content.trim().is_empty() {
            return Err(AppError::BadRequest("content cannot be empty".into()));
        }
    }

    // Build dynamic update
    let title = form.title.as_deref();
    let content = form.content.as_deref();

    match (title, content) {
        (Some(t), Some(c)) => {
            sqlx::query("UPDATE pastes SET title = ?, content = ? WHERE id = ?")
                .bind(t)
                .bind(c)
                .bind(&id)
                .execute(&state.db)
                .await?;
        }
        (Some(t), None) => {
            sqlx::query("UPDATE pastes SET title = ? WHERE id = ?")
                .bind(t)
                .bind(&id)
                .execute(&state.db)
                .await?;
        }
        (None, Some(c)) => {
            sqlx::query("UPDATE pastes SET content = ? WHERE id = ?")
                .bind(c)
                .bind(&id)
                .execute(&state.db)
                .await?;
        }
        (None, None) => {}
    }

    Ok(StatusCode::NO_CONTENT)
}
