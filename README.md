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

### Docker Compose (recommended)

```
docker compose up -d
```

Edit `docker-compose.yml` to change `PASTEBIN_USER` / `PASTEBIN_PASS` and other settings. Uncomment `build: .` to build from source instead of pulling the image.

### Docker run

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

JavaScript content
------------------
Pastes support `kind=text` (the default, including existing records) and
`kind=javascript`. JavaScript runs on the server using embedded `deno_runtime`
(V8); no Deno CLI or Node installation is required. The Docker build includes the
runtime in the application executable.

In the create/edit page, select **JavaScript** and use **Run preview** to execute
unsaved code. The result, console logs, errors, and elapsed time appear on the
page. Preview makes real HTTP requests, including POST requests. Saving code does
not require a successful preview.

Create pastes at `/new`; use **Edit paste** to open `/pastes/<id>/edit`.
Both are full pages. The editor, preview, and current cached result each have a
**Full screen** toggle; press **Esc** to return to the page without losing changes.
The edit page also supports Public/Private visibility, changing or generating an
access key, and changing/removing the expiration. Saving returns to the detail
page with the updated Raw URL. Rotating a private key invalidates old private
links; switching to Public removes the key requirement.

Scripts are JavaScript ES modules with a **default-exported function**. The server
loads the module and calls that function once, awaiting its result. Synchronous
and async functions are supported. Return a **string** (including an empty
string); use `JSON.stringify` for JSON output. `console.log/info/warn/error/debug`
appear separately in preview logs. Top-level `return` scripts are no longer
supported: move their code into `export default async function () { ... }`.

```js
export default async function () {
  const response = await fetch("https://api.example.com/items", {
    headers: { Accept: "application/json" }
  });
  if (!response.ok) throw new Error(`Upstream HTTP ${response.status}`);
  const data = await response.json();
  console.log("Items:", data.items.length);
  return data.items.map(item => item.url).join("\n");
}
```

### HTTPS ESM dependencies

Use a full HTTPS URL to a JavaScript ESM build. Static `import` declarations,
`export ... from`, and dynamic `await import(url)` are supported. Dependencies
can import relative URLs; after a redirect they resolve against the final URL.
For example, parsing and generating YAML with a version-pinned dependency:

```js
import { load, dump } from "https://cdn.jsdelivr.net/npm/js-yaml@4.1.1/dist/js-yaml.mjs";

export default function () {
  const config = load("name: demo\ninterval: 5\n");
  config.interval = 10;
  return dump(config);
}
```

Modules download automatically on first use. Downloaded source is cached in
memory across preview, Raw, and scheduled executions for up to one hour (128
entries / 32 MiB, oldest entries evicted first). Restarting clears this dependency
cache; it is separate from the scheduled **result** cache in SQLite. Each run
still evaluates modules in a fresh isolate, so module globals never persist.
Pin versions in URLs for predictable dependencies. Failed downloads are not cached.

Imports follow `PASTEBIN_JS_ALLOW_NET` and the public-network restrictions,
including every redirect and DNS lookup. Add dependency CDN hosts and any
redirect/dependency hosts to your allowlist. Downloads connect directly, without
using proxy environment variables. Limits: 64 modules and 8 MiB combined source
per execution, 2 MiB per module, 5 redirects, and 10 seconds per download including
redirects and body. Loading counts toward the overall 30-second execution limit.
Servers must return a JavaScript Content-Type and UTF-8 source.

This is an HTTPS ESM loader, not Deno CLI package resolution. Bare package names,
`npm:`, `jsr:`, local files, HTTP imports, TypeScript, JSX, CommonJS, JSON/Wasm
imports, browser DOM, and `Deno`/Node system namespaces are not supported. Use
JavaScript ESM builds that work with the available web APIs (`fetch`, timers,
Web Crypto, etc.).

### Execution modes

- **Run on every Raw request**: omit `scheduler`, or set it to an empty string.
  Each authorized `/raw/<id>` request executes the stored script and returns its
  output. Execution failures return HTTP 502 without exposing script diagnostics.
- **Schedule and cache**: set `scheduler` to a five-field Cron expression, such as
  `*/5 * * * *` (every five minutes) or `0 9 * * *` (daily at 09:00). The fields are
  minute, hour, day of month, month, and weekday. The server runs the script once
  shortly after creation and then on schedule. Raw requests only read the most
  recent successful result; they do not run the script.

Cached content and execution status are stored in SQLite and survive restart.
A failed scheduled execution preserves the last successful result. Before the
first successful execution, Raw returns HTTP 503. The detail page shows the last
attempt, last success, next scheduled time, and latest error; refresh it to see
new status. Preview never writes the cache.

Changing code, type, or schedule invalidates the old cache and queues an initial
run when still scheduled. Changing only the title preserves it. In-flight results
from an older revision are discarded. Expired/deleted pastes stop being scheduled.
Missed occurrences during downtime are coalesced into one execution after startup;
they are not replayed. A database lease prevents overlapping claims for the same
revision; after a crash, a claimed run can be retried when its two-minute lease
expires. External side effects are **not** guaranteed exactly once.

| Environment | Default | Meaning |
|---|---|---|
| `PASTEBIN_JS_TIMEZONE` | `Asia/Shanghai` | IANA time zone used for all Cron schedules, e.g. `UTC` |
| `PASTEBIN_JS_ALLOW_NET` | all public hosts | Optional comma-separated allowed hosts, optionally with ports, e.g. `api.example.com:443,example.org` |

Private, loopback, link-local, and reserved network ranges are blocked, including
literal IPs, redirect destinations, and DNS results. `PASTEBIN_JS_ALLOW_NET` narrows
public access; it does not override the private-network restriction. Network
requests originate from the server and are not subject to browser CORS.

Limits per execution: 30 seconds total, 10 `fetch` calls, 10 seconds per fetch
including its body, 5 MiB per response, 256 KiB source, 1 MiB output, and 64 KiB logs.
Fetch responses are buffered under the size cap, so this is not a streaming Fetch
implementation. At most two scripts execute concurrently. V8 uses a 64 MiB heap
limit with interruption near the limit; this is not a cap on total process memory.
Only administrators should author scripts: embedded isolates share the server
process and do not provide OS-level fault isolation.

### API examples

Create a JavaScript paste that runs on each Raw request:

```sh
curl -H 'X-User: admin' -H 'X-Pass: secret' \
  --data-urlencode 'kind=javascript' \
  --data-urlencode 'content=export default () => new Date().toISOString();' \
  http://127.0.0.1:8080/api/pastes
```

Create a scheduled JavaScript paste:

```sh
curl -H 'X-User: admin' -H 'X-Pass: secret' \
  --data-urlencode 'kind=javascript' \
  --data-urlencode 'scheduler=*/5 * * * *' \
  --data-urlencode 'content=export default () => new Date().toISOString();' \
  http://127.0.0.1:8080/api/pastes
```

Preview without saving:

```sh
curl -H 'X-User: admin' -H 'X-Pass: secret' -H 'Content-Type: application/json' \
  -d '{"content":"export default () => { console.log(\"hello\"); return \"result\"; };"}' \
  http://127.0.0.1:8080/api/script-preview
```

Preview returns `{output, logs, error, duration_ms}`. Script errors are reported
in `error`; authentication/request errors use HTTP status codes. `PUT
/api/pastes/<id>` accepts `kind` and `scheduler` alongside `title` and `content`.
Omitted update fields are preserved; `"scheduler":""` switches to per-request
execution. Detail responses include `scheduler`, `last_run_at`, `last_error`,
`cache_updated_at`, and `next_run_at`; timestamps are RFC 3339 UTC.

Updates also accept `visibility` (`public` or `private`), `access_password`, and
either `expires_at` (a future RFC 3339 timestamp) or `expires_in` (positive seconds).
Use `"expires_at":""` to remove expiration. Omit expiration fields to keep the
existing deadline. For Private pastes, omitting `access_password` preserves the
current key (generating one when switching from Public); an empty string
generates a new key. Public pastes have no access key. Access and expiration
changes preserve scheduled results and do not rerun the script.

### Building the embedded runtime

Use Rust 1.98 or newer and the committed `Cargo.lock` (`cargo build --locked`).
Deno, V8, SQLx, and ICU versions are selected together: both Deno and SQLx link
SQLite, and this V8 release depends on ICU 2.1 unstable APIs. Update these
constraints as a set. The first build downloads V8's prebuilt archive and is
substantially larger than the plain-text-only application. Linux builds need
Clang/libclang, CMake, Python 3, and a C/C++ toolchain; the Dockerfile installs them.
