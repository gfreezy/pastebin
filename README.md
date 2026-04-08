Pastebin (Rust + SQLite)
========================

Features
--------
- Session-based admin login with optional TOTP 2FA (enrollable from the UI)
- Public/private pastes; private auto-generates an access key for raw URLs
- Custom paste IDs supported
- Inline view & edit pages
- File upload (drag & drop or file picker)
- Expiry support (expires_in or expires_at); never-expire by default
- SQLite storage

Environment
-----------
| Variable | Required | Default |
|---|---|---|
| `PASTEBIN_USER` | yes | |
| `PASTEBIN_PASS` | yes | |
| `PASTEBIN_DB_PATH` | no | `pastebin.db` |
| `PASTEBIN_BIND` | no | `127.0.0.1:8080` |
| `PASTEBIN_BASE_URL` | no | `http://{PASTEBIN_BIND}` |

Run
---
```
export PASTEBIN_USER=admin
export PASTEBIN_PASS=secret
cargo run
```

Docker
------

### Pull from GitHub Container Registry

```
docker pull ghcr.io/gfreezy/pastebin:main
```

> Replace `OWNER` with your GitHub username or org.

### Run

```
docker run -d \
  -p 8080:8080 \
  -v pastebin-data:/data \
  -e PASTEBIN_USER=admin \
  -e PASTEBIN_PASS=secret \
  ghcr.io/gfreezy/pastebin:main
```

### Build locally

```
docker build -t pastebin .
docker run -d -p 8080:8080 -v pastebin-data:/data \
  -e PASTEBIN_USER=admin -e PASTEBIN_PASS=secret pastebin
```

The SQLite database is stored at `/data/pastebin.db` inside the container. Mount a volume to persist data.

API (curl)
----------
API endpoints use header-based auth: `X-User` and `X-Pass`. If TOTP is enabled, also include `X-TOTP`.

Create public paste:
```
curl -H "X-User: admin" -H "X-Pass: secret" \
  -d "content=hello world" \
  -d "visibility=public" \
  http://127.0.0.1:8080/api/pastes
```

Create private paste:
```
curl -H "X-User: admin" -H "X-Pass: secret" \
  -d "content=private data" \
  -d "visibility=private" \
  http://127.0.0.1:8080/api/pastes
```

Create with custom ID and expiry:
```
curl -H "X-User: admin" -H "X-Pass: secret" \
  -d "content=expiring" \
  -d "id=my-custom-id" \
  -d "expires_in=3600" \
  http://127.0.0.1:8080/api/pastes
```

Raw access
----------
Public:
```
curl http://127.0.0.1:8080/raw/<id>
```

Private:
```
curl "http://127.0.0.1:8080/raw/<id>?key=<access_key>"
```

List pastes
-----------
```
curl -H "X-User: admin" -H "X-Pass: secret" http://127.0.0.1:8080/api/pastes
```
