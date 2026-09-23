mod auth;
mod db;
mod error;
mod handlers;
mod models;
mod scheduler;
mod script;

use axum::{
    Router, middleware,
    routing::{get, post},
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use std::collections::HashMap;
use std::sync::Mutex;
use std::{net::SocketAddr, sync::Arc};
use tower_http::trace::TraceLayer;
use tracing::info;

use models::AppState;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt().with_env_filter("info").init();

    let admin_user = std::env::var("PASTEBIN_USER").expect("PASTEBIN_USER is required");
    let admin_pass = std::env::var("PASTEBIN_PASS").expect("PASTEBIN_PASS is required");
    let db_path = std::env::var("PASTEBIN_DB_PATH").unwrap_or_else(|_| "pastebin.db".to_string());
    let bind_addr = std::env::var("PASTEBIN_BIND").unwrap_or_else(|_| "127.0.0.1:8080".to_string());
    let base_url =
        std::env::var("PASTEBIN_BASE_URL").unwrap_or_else(|_| format!("http://{bind_addr}"));

    let options = SqliteConnectOptions::new()
        .filename(&db_path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal);
    let db = sqlx::SqlitePool::connect_with(options).await?;
    db::init_db(&db).await?;

    let scripts = script::ScriptExecutor::from_env()?;
    let script_timezone = std::env::var("PASTEBIN_JS_TIMEZONE")
        .unwrap_or_else(|_| "Asia/Shanghai".to_string())
        .parse::<chrono_tz::Tz>()?;
    let state = Arc::new(AppState {
        scripts,
        script_timezone,
        db,
        admin_user,
        admin_pass,
        base_url,
        sessions: Mutex::new(HashMap::new()),
    });

    tokio::spawn(scheduler::run(state.clone()));

    let admin_routes = Router::new()
        .route("/", get(handlers::admin_index))
        .route("/pastes/:id", get(handlers::view_paste))
        .route(
            "/totp-setup",
            get(auth::totp_setup_page).post(auth::totp_setup_submit),
        )
        .route("/totp-disable", post(auth::totp_disable))
        .route("/api/script-preview", post(handlers::preview_script))
        .route(
            "/api/pastes",
            get(handlers::list_pastes).post(handlers::create_paste),
        )
        .route(
            "/api/pastes/:id",
            get(handlers::get_paste)
                .put(handlers::update_paste)
                .delete(handlers::delete_paste),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::admin_auth,
        ));

    let app = Router::new()
        .route("/login", get(auth::login_page).post(auth::login_submit))
        .route("/logout", get(auth::logout))
        .route("/raw/:id", get(handlers::raw_paste))
        .merge(admin_routes)
        .with_state(state)
        .layer(TraceLayer::new_for_http());

    let addr: SocketAddr = bind_addr.parse()?;
    info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
