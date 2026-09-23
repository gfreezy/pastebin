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
  const nativeFetch = globalThis.fetch;
  let requests = 0;
  globalThis.fetch = async (input, init) => {
    if (++requests > 10) throw new Error('At most 10 fetch calls are allowed per execution');
    const request = new Request(input, init);
    const url = new URL(request.url);
    if (!['http:', 'https:'].includes(url.protocol)) throw new Error('Only HTTP and HTTPS requests are allowed');
    const signal = AbortSignal.any([request.signal, AbortSignal.timeout(10000)]);
    const response = await nativeFetch(request, { signal });
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
  // Host permissions also deny system I/O; module imports use NoopModuleLoader.
  for (const name of ['Deno', 'process', 'Worker', 'WebSocket', 'WebSocketStream', 'EventSource', 'BroadcastChannel', 'navigator', 'close']) {
    Object.defineProperty(globalThis, name, { value: undefined, writable: false, configurable: false });
  }
})();
