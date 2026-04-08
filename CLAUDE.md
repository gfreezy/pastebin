# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build & Run

```bash
# Build
cargo build

# Run (requires PASTEBIN_USER and PASTEBIN_PASS env vars)
PASTEBIN_USER=admin PASTEBIN_PASS=secret cargo run

# Or use a .env file
cargo run
```

Optional env vars: `PASTEBIN_DB_PATH` (default: `pastebin.db`), `PASTEBIN_BIND` (default: `127.0.0.1:8080`), `PASTEBIN_BASE_URL` (default: `http://{bind_addr}`), `PASTEBIN_TOTP_SECRET` (base32, enables TOTP 2FA).

No tests exist yet. If added, run with `cargo test`.

## Architecture

Rust web application using **axum** + **SQLite** (via sqlx) + **tokio** + **askama** templates.

### Module layout

| Module | Purpose |
|---|---|
| `main.rs` | Startup, router wiring, env config |
| `models.rs` | AppState, Visibility, form/response structs, helper fns (parse_visibility, parse_expiry, now_ts, format_ts, build_raw_url) |
| `error.rs` | AppError enum + IntoResponse |
| `auth.rs` | admin_auth middleware, session management, TOTP verification, login/logout/totp-setup handlers |
| `handlers.rs` | Paste CRUD handlers + askama IndexTemplate/ViewTemplate |
| `db.rs` | init_db, purge_expired, fetch_paste_meta |
| `templates/index.html` | Admin dashboard (HTML + JS) |
| `templates/view.html` | Paste view/edit page |
| `templates/login.html` | Login form |
| `templates/totp_setup.html` | TOTP QR code setup page |

### Route Layout

| Route | Auth | Purpose |
|---|---|---|
| `GET /login` | None | Login page |
| `POST /login` | None | Login submit |
| `GET /logout` | None | Clear session |
| `GET /` | Session/Basic | Admin dashboard |
| `GET /pastes/:id` | Session/Basic | View/edit paste |
| `GET /totp-setup` | Session/Basic | TOTP enrollment (QR code) |
| `POST /api/pastes` | Session/Basic | Create paste |
| `GET /api/pastes` | Session/Basic | List pastes (JSON) |
| `GET /api/pastes/:id` | Session/Basic | Paste detail (JSON) |
| `PUT /api/pastes/:id` | Session/Basic | Update paste |
| `DELETE /api/pastes/:id` | Session/Basic | Delete paste |
| `GET /raw/:id` | None | Raw paste content; private pastes require `?key=` |

### Key Design Decisions

- **askama** for compile-time checked HTML templates with auto-escaping. JS blocks wrapped in `{% raw %}`.
- **Session-based auth** for browser (login form → cookie). Basic Auth fallback for API/curl with `X-TOTP` header when TOTP is enabled.
- **TOTP 2FA** (opt-in via `PASTEBIN_TOTP_SECRET` env var). Uses `totp-rs` for verification, `qrcode` for QR SVG on setup page.
- **In-memory sessions** — `HashMap<token, expiry>` in `AppState`, 24h TTL, cleaned up on each login.
- **Access keys** stored plaintext in `access_key` column (not hashed) so raw URLs always include the real key. Custom paste IDs supported, auto-generated random key for private pastes.
- **Expiry** — pastes can have `expires_in` (seconds) or `expires_at` (RFC3339). Expired pastes are purged lazily on each request via `purge_expired()`.
- **nanoid(10)** for paste IDs (or custom), **nanoid(16)** for auto-generated access keys.
- **SQLite WAL mode** enabled at startup for concurrency. Single `pastes` table with indexes on `expires_at` and `created_at`.
- **AppState** (db pool, admin creds, base_url, totp_secret, sessions) shared via `Arc<AppState>`.
