import { op_paste_log } from 'ext:core/ops';
globalThis.__pasteLog = op_paste_log;

import { op_paste_http_client } from 'ext:core/ops';
import { HttpClient } from 'ext:deno_fetch/22_http_client.js';
import { loadTlsKeyPair } from 'ext:deno_net/02_tls.js';
globalThis.__pasteHttpClient = options => new HttpClient(
  op_paste_http_client(options.caCerts, loadTlsKeyPair('Deno.createHttpClient', options), options.http1, options.http2),
);
