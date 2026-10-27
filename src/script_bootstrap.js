(() => {
  const log = globalThis.__pasteLog;
  delete globalThis.__pasteLog;
  const encode = value => {
    if (typeof value === 'string') return value;
    if (value instanceof Error) return value.stack || value.message;
    try { return JSON.stringify(value) ?? String(value); }
    catch { return String(value); }
  };
  for (const level of ['log', 'info', 'warn', 'error', 'debug']) {
    console[level] = (...args) => log(`[${level}] ${args.map(encode).join(' ')}`);
  }
  const nativeCreateClient = globalThis.__pasteHttpClient;
  delete globalThis.__pasteHttpClient;
  const clients = new WeakMap();
  let clientCount = 0;
  const createHttpClient = (options = {}) => {
    if (!options || typeof options !== 'object' || Array.isArray(options)) {
      throw new TypeError('HTTP client options must be an object');
    }
    for (const name of Reflect.ownKeys(options)) {
      if (!['cert', 'key', 'caCerts', 'http1', 'http2'].includes(name)) {
        throw new TypeError('Only cert, key, caCerts, http1, and http2 HTTP client options are supported');
      }
    }
    const { cert, key, caCerts = [], http1 = true, http2 = true } = options;
    if (typeof http1 !== 'boolean' || typeof http2 !== 'boolean') {
      throw new TypeError('http1 and http2 must be booleans');
    }
    if (!http1 && !http2) throw new TypeError('At least one of http1 or http2 must be enabled');
    if ((cert !== undefined && typeof cert !== 'string') ||
        (key !== undefined && typeof key !== 'string') ||
        !Array.isArray(caCerts) || caCerts.some(value => typeof value !== 'string')) {
      throw new TypeError('cert, key, and caCerts entries must be PEM strings');
    }
    if (clientCount >= 10) throw new Error('At most 10 HTTP clients are allowed per execution');
    const native = nativeCreateClient({ __proto__: null, cert, key, caCerts: [...caCerts], http1, http2 });
    clientCount++;
    const client = Object.freeze({
      close: () => native.close(),
      [Symbol.dispose]: () => native[Symbol.dispose](),
    });
    clients.set(client, native);
    return client;
  };
  Object.defineProperty(globalThis, 'Deno', {
    value: Object.freeze({ createHttpClient }), writable: false, configurable: false,
  });
  const nativeFetch = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = async (input, init) => {
    if (++requests > 10) throw new Error('At most 10 fetch calls are allowed per execution');
    const client = init?.client;
    if (client !== undefined && !clients.has(client)) {
      throw new TypeError('client must be created by Deno.createHttpClient');
    }
    const request = new Request(input, { ...init, client: clients.get(client) });
    const url = new URL(request.url);
    if (!['http:', 'https:'].includes(url.protocol)) throw new Error('Only HTTP and HTTPS requests are allowed');
    const signal = AbortSignal.any([request.signal, AbortSignal.timeout(10000)]);
    const response = await nativeFetch(request, { signal, client: clients.get(client) });
    // Buffer under a cap so text(), json(), and callers ignoring the body cannot
    // accumulate unbounded response data. Timeout covers body consumption too.
    const reader = response.body?.getReader();
    const chunks = [];
    let length = 0;
    if (reader) {
      try {
        while (true) {
          const { done, value } = await reader.read();
          if (done) break;
          length += value.byteLength;
          if (length > 5 * 1024 * 1024) throw new Error('Response exceeds 5 MiB');
          chunks.push(value);
        }
      } finally { await reader.cancel().catch(() => {}); }
    }
    const result = new Response(reader ? new Blob(chunks) : null, {
      status: response.status, statusText: response.statusText, headers: response.headers,
    });
    Object.defineProperties(result, {
      url: { value: response.url }, redirected: { value: response.redirected }, type: { value: response.type },
    });
    return result;
  };
  // Do not expose process exit, stdio, filesystem, sockets, or alternate clients.
  // Host permissions also deny system I/O; module imports use the HTTPS loader.
  for (const name of ['process', 'Worker', 'WebSocket', 'WebSocketStream', 'EventSource', 'BroadcastChannel', 'navigator', 'close']) {
    Object.defineProperty(globalThis, name, { value: undefined, writable: false, configurable: false });
  }
})();
