use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::sync::Mutex;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

use crate::error::AppError;

pub struct AppState {
    pub db: SqlitePool,
    pub scripts: crate::script::ScriptExecutor,
    pub script_timezone: chrono_tz::Tz,
    pub admin_user: String,
    pub admin_pass: String,
    pub base_url: String,
    pub sessions: Mutex<HashMap<String, i64>>,
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
    pub totp_code: Option<String>,
}

#[derive(Deserialize)]
pub struct TotpVerifyForm {
    pub code: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Visibility::Public => "public",
            Visibility::Private => "private",
        }
    }
}

#[derive(Deserialize)]
pub struct CreatePasteForm {
    pub scheduler: Option<String>,
    pub kind: Option<String>,
    pub id: Option<String>,
    pub content: String,
    pub title: Option<String>,
    pub visibility: Option<String>,
    pub access_password: Option<String>,
    pub expires_in: Option<i64>,
    pub expires_at: Option<String>,
}

#[derive(Deserialize)]
pub struct RawQuery {
    pub key: Option<String>,
}

#[derive(Serialize)]
pub struct PasteMeta {
    pub scheduler: Option<String>,
    pub kind: String,
    pub id: String,
    pub title: Option<String>,
    pub visibility: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub raw_url: String,
    pub needs_key: bool,
}

#[derive(Serialize)]
pub struct PasteDetail {
    pub cached_content: Option<String>,
    pub cache_updated_at: Option<String>,
    pub last_run_at: Option<String>,
    pub last_error: Option<String>,
    pub next_run_at: Option<String>,
    pub scheduler: Option<String>,
    pub kind: String,
    pub id: String,
    pub title: Option<String>,
    pub visibility: String,
    pub content: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub raw_url: String,
    pub needs_key: bool,
}

#[derive(Serialize)]
pub struct CreatePasteResponse {
    pub scheduler: Option<String>,
    pub kind: String,
    pub id: String,
    pub visibility: String,
    pub created_at: String,
    pub expires_at: Option<String>,
    pub raw_url: String,
}

#[derive(Deserialize)]
pub struct UpdatePasteForm {
    pub visibility: Option<String>,
    pub access_password: Option<String>,
    pub expires_at: Option<String>,
    pub expires_in: Option<i64>,
    pub scheduler: Option<String>,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
}

pub fn parse_visibility(input: Option<String>) -> Result<Visibility, AppError> {
    let value = input.unwrap_or_else(|| "public".to_string());
    parse_visibility_string(value)
}

pub fn parse_visibility_string(value: String) -> Result<Visibility, AppError> {
    match value.to_lowercase().as_str() {
        "public" => Ok(Visibility::Public),
        "private" => Ok(Visibility::Private),
        other => Err(AppError::BadRequest(format!("invalid visibility: {other}"))),
    }
}

pub fn parse_expiry(
    expires_at: Option<&str>,
    expires_in: Option<i64>,
) -> Result<Option<i64>, AppError> {
    if expires_at.is_some() && expires_in.is_some() {
        return Err(AppError::BadRequest(
            "Use expires_at or expires_in, not both".into(),
        ));
    }
    if let Some(expires_at) = expires_at {
        if expires_at.is_empty() {
            return Ok(None);
        }
        let parsed = OffsetDateTime::parse(expires_at, &Rfc3339)
            .map_err(|_| AppError::BadRequest("expires_at must be RFC3339".into()))?;
        if parsed.unix_timestamp() <= now_ts() {
            return Err(AppError::BadRequest(
                "Expiration must be in the future".into(),
            ));
        }
        return Ok(Some(parsed.unix_timestamp()));
    }

    if let Some(expires_in) = expires_in {
        if expires_in <= 0 {
            return Err(AppError::BadRequest("expires_in must be > 0".into()));
        }
        return now_ts()
            .checked_add(expires_in)
            .filter(|ts| OffsetDateTime::from_unix_timestamp(*ts).is_ok())
            .map(Some)
            .ok_or_else(|| AppError::BadRequest("expires_in is too large".into()));
    }

    Ok(None)
}

pub fn now_ts() -> i64 {
    OffsetDateTime::now_utc().unix_timestamp()
}

pub fn format_ts(ts: i64) -> String {
    OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|dt| dt.format(&Rfc3339).ok())
        .unwrap_or_else(|| ts.to_string())
}

pub fn build_raw_url(
    base_url: &str,
    id: &str,
    visibility: Visibility,
    key: Option<&str>,
) -> String {
    let base = base_url.trim_end_matches('/');
    match visibility {
        Visibility::Public => format!("{base}/raw/{id}"),
        Visibility::Private => {
            let key = key.unwrap_or("YOUR_PASSWORD");
            let key = urlencoding::encode(key);
            format!("{base}/raw/{id}?key={key}")
        }
    }
}

/// Missing types preserve the original plain-text API behavior.
pub fn parse_kind(value: Option<&str>) -> Result<&str, AppError> {
    match value.unwrap_or("text") {
        "text" => Ok("text"),
        "javascript" => Ok("javascript"),
        other => Err(AppError::BadRequest(format!("invalid kind: {other}"))),
    }
}
