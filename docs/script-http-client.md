# Script HTTP clients (mTLS)

Scripts can create a restricted HTTP client and pass it to `fetch`:

```js
export default async function () {
  const cert = `-----BEGIN CERTIFICATE-----
...client certificate PEM...
-----END CERTIFICATE-----`;
  const key = `-----BEGIN PRIVATE KEY-----
...client private key PEM...
-----END PRIVATE KEY-----`;
  const client = Deno.createHttpClient({ cert, key });
  try {
    const response = await fetch("https://api.example.com/data", { client });
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    return await response.text();
  } finally {
    client.close();
  }
}
```

- Only `cert`, `key`, `caCerts`, `http1`, and `http2` options are accepted. `cert` and `key`
  must be provided together as PEM strings. `caCerts` is an optional array of
  PEM strings for additional trusted certificate authorities.
- `http1` and `http2` are booleans, both defaulting to `true`. At least one
  must be enabled. Use `{ cert, key, http1: true, http2: false }` for HTTP/1.1 only.
- `Deno` exposes only `createHttpClient`. Filesystem, environment, sockets,
  process APIs, proxy options, and disabling TLS verification are unavailable.
- `fetch(url, { client })` accepts clients created by this API. The returned
  handle provides `close()` and `[Symbol.dispose]()`. Close it after consuming
  the response. Up to 10 clients may be created per execution.
- Existing host allowlists, public-address DNS checks (including redirects),
  execution timeout, 10-fetch limit, 10-second fetch timeout, and 5 MiB response
  limit still apply. Client resources are scoped to one script execution.
- The embedded Deno client builder automatically reads proxy environment
  variables. To prevent proxy-side DNS from bypassing address checks, client
  creation fails if a nonempty `HTTP_PROXY`, `HTTPS_PROXY`, or `ALL_PROXY`
  environment variable is present (including lowercase variants).

This is a restricted Deno extension, not a browser Fetch API feature. Client
certificates apply to fetch requests using that client, not to ESM downloads.
