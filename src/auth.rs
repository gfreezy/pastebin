use askama::Template;
use axum::{
    body::Body,
    extract::{Form, State},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Redirect, Response},
};
use std::sync::Arc;
use totp_rs::{Algorithm, TOTP, Secret};

use crate::db::{get_setting, set_setting, delete_setting};
use crate::models::{AppState, LoginForm, TotpVerifyForm, now_ts};

const SESSION_TTL: i64 = 86400; // 24 hours
const SESSION_COOKIE: &str = "session";

// --- Templates ---

#[derive(Template)]
#[template(path = "login.html")]
struct LoginTemplate {
    totp_enabled: bool,
    error: Option<String>,
}

#[derive(Template)]
#[template(path = "totp_setup.html")]
struct TotpSetupTemplate {
    secret: String,
    qr_svg: String,
    enrolled: bool,
    error: Option<String>,
}

// --- Middleware ---

pub async fn admin_auth(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // 1. Check session cookie
    if check_session_cookie(req.headers(), &state) {
        return next.run(req).await;
    }

    // 2. Fall back to header auth (for API/curl): X-User + X-Pass + X-TOTP
    if check_header_auth(req.headers(), &state).await {
        return next.run(req).await;
    }

    let is_api = req.uri().path().starts_with("/api/");
    if is_api {
        (StatusCode::UNAUTHORIZED, "Unauthorized").into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

fn check_session_cookie(headers: &HeaderMap, state: &AppState) -> bool {
    let token = extract_session_token(headers);
    let Some(token) = token else { return false };

    let sessions = state.sessions.lock().unwrap();
    if let Some(&expiry) = sessions.get(token) {
        expiry > now_ts()
    } else {
        false
    }
}

fn extract_session_token<'a>(headers: &'a HeaderMap) -> Option<&'a str> {
    let cookie_str = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie_str
        .split(';')
        .filter_map(|c| c.trim().strip_prefix("session="))
        .next()
}

async fn check_header_auth(headers: &HeaderMap, state: &AppState) -> bool {
    let user = headers.get("x-user").and_then(|v| v.to_str().ok());
    let pass = headers.get("x-pass").and_then(|v| v.to_str().ok());

    let (Some(user), Some(pass)) = (user, pass) else {
        return false;
    };
    if user != state.admin_user || pass != state.admin_pass {
        return false;
    }

    // Check TOTP if enrolled
    if let Ok(Some(secret)) = get_setting(&state.db, "totp_secret").await {
        let code = headers.get("x-totp").and_then(|v| v.to_str().ok());
        let Some(code) = code else { return false };
        return verify_totp(&secret, code);
    }

    true
}

// --- Session helpers ---

fn create_session(state: &AppState) -> String {
    let token = nanoid::nanoid!(32);
    let expiry = now_ts() + SESSION_TTL;
    let mut sessions = state.sessions.lock().unwrap();
    sessions.retain(|_, &mut exp| exp > now_ts());
    sessions.insert(token.clone(), expiry);
    token
}

fn set_session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL}")
}

fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

// --- TOTP helpers ---

fn build_totp(secret_base32: &str, account_name: &str) -> Option<TOTP> {
    let secret = Secret::Encoded(secret_base32.to_string())
        .to_bytes()
        .ok()?;
    TOTP::new(
        Algorithm::SHA1, 6, 1, 30, secret,
        Some("Pastebin".to_string()),
        account_name.to_string(),
    ).ok()
}

fn verify_totp(secret_base32: &str, code: &str) -> bool {
    let Some(totp) = build_totp(secret_base32, "") else { return false };
    totp.check_current(code).unwrap_or(false)
}

fn generate_totp_secret() -> String {
    let secret = Secret::generate_secret();
    secret.to_encoded().to_string()
}

fn render_qr_svg(totp: &TOTP) -> Result<String, StatusCode> {
    let uri = totp.get_url();
    let qr = qrcode::QrCode::new(uri.as_bytes())
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(qr.render::<qrcode::render::svg::Color>()
        .min_dimensions(200, 200)
        .build())
}

// --- Handlers ---

pub async fn login_page(
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    let totp_enrolled = get_setting(&state.db, "totp_secret").await
        .ok().flatten().is_some();
    LoginTemplate { totp_enabled: totp_enrolled, error: None }
}

pub async fn login_submit(
    State(state): State<Arc<AppState>>,
    Form(form): Form<LoginForm>,
) -> Response {
    let totp_secret = get_setting(&state.db, "totp_secret").await
        .ok().flatten();
    let totp_enrolled = totp_secret.is_some();

    if form.username != state.admin_user || form.password != state.admin_pass {
        return LoginTemplate {
            totp_enabled: totp_enrolled,
            error: Some("Invalid username or password".into()),
        }.into_response();
    }

    if let Some(ref secret) = totp_secret {
        let code = form.totp_code.as_deref().unwrap_or("");
        if !verify_totp(secret, code) {
            return LoginTemplate {
                totp_enabled: true,
                error: Some("Invalid TOTP code".into()),
            }.into_response();
        }
    }

    let token = create_session(&state);
    let cookie = set_session_cookie(&token);
    let redirect = if totp_enrolled { "/" } else { "/totp-setup" };
    ([(header::SET_COOKIE, cookie)], Redirect::to(redirect)).into_response()
}

pub async fn logout(
    State(state): State<Arc<AppState>>,
    req: Request<Body>,
) -> Response {
    if let Some(token) = extract_session_token(req.headers()) {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.remove(token);
    }
    ([(header::SET_COOKIE, clear_session_cookie())], Redirect::to("/login")).into_response()
}

pub async fn totp_setup_page(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, StatusCode> {
    let enrolled = get_setting(&state.db, "totp_secret").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // If already enrolled, show existing secret
    if let Some(secret) = enrolled {
        let totp = build_totp(&secret, &state.admin_user)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let qr_svg = render_qr_svg(&totp)?;
        return Ok(TotpSetupTemplate { secret, qr_svg, enrolled: true, error: None });
    }

    // Generate or reuse pending secret
    let pending = get_setting(&state.db, "totp_pending").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let secret = if let Some(s) = pending {
        s
    } else {
        let s = generate_totp_secret();
        set_setting(&state.db, "totp_pending", &s).await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        s
    };

    let totp = build_totp(&secret, &state.admin_user)
        .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
    let qr_svg = render_qr_svg(&totp)?;

    Ok(TotpSetupTemplate { secret, qr_svg, enrolled: false, error: None })
}

pub async fn totp_setup_submit(
    State(state): State<Arc<AppState>>,
    Form(form): Form<TotpVerifyForm>,
) -> Result<Response, StatusCode> {
    // If already enrolled, reject
    let enrolled = get_setting(&state.db, "totp_secret").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    if enrolled.is_some() {
        return Ok(Redirect::to("/totp-setup").into_response());
    }

    let pending = get_setting(&state.db, "totp_pending").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let Some(secret) = pending else {
        return Ok(Redirect::to("/totp-setup").into_response());
    };

    // Verify code
    if !verify_totp(&secret, &form.code) {
        let totp = build_totp(&secret, &state.admin_user)
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?;
        let qr_svg = render_qr_svg(&totp)?;
        return Ok(TotpSetupTemplate {
            secret,
            qr_svg,
            enrolled: false,
            error: Some("Invalid code. Please try again.".into()),
        }.into_response());
    }

    // Enroll: move pending → secret
    set_setting(&state.db, "totp_secret", &secret).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    delete_setting(&state.db, "totp_pending").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Redirect::to("/").into_response())
}

pub async fn totp_disable(
    State(state): State<Arc<AppState>>,
) -> Result<Response, StatusCode> {
    delete_setting(&state.db, "totp_secret").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    delete_setting(&state.db, "totp_pending").await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Redirect::to("/").into_response())
}
