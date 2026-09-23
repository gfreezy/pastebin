//! Embedded Deno executor. Only administrators author scripts; public raw URLs
//! may execute them. Each invocation has its own isolate and bounded lifetime.
use deno_runtime::deno_core::{self, JsRuntime, ModuleSpecifier, OpState, op2, v8};
use deno_runtime::deno_permissions::{
    Host, NetDescriptor, PermissionDescriptorParser, Permissions, PermissionsContainer,
    PermissionsOptions, RuntimePermissionDescriptorParser,
};
use deno_runtime::worker::{MainWorker, WorkerOptions, WorkerServiceOptions};
use serde::Serialize;
use std::{
    net::IpAddr,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
mod modules;

const TIMEOUT: Duration = Duration::from_secs(30);
pub const MAX_SOURCE: usize = 256 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;
const MAX_LOGS: usize = 64 * 1024;
// Applied both to literal addresses (Deno permissions) and resolved addresses.
const BLOCKED_NETS: &[&str] = &[
    "0.0.0.0/8",
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.0.0.0/24",
    "192.0.2.0/24",
    "192.168.0.0/16",
    "198.18.0.0/15",
    "198.51.100.0/24",
    "203.0.113.0/24",
    "224.0.0.0/4",
    "240.0.0.0/4",
    "::/3",
    "4000::/2",
    "8000::/1",
    "2001::/23",
    "2001:db8::/32",
    "2002::/16",
    "3fff::/20",
];

#[derive(Clone)]
pub struct ScriptExecutor {
    slots: Arc<Semaphore>,
    permissions: PermissionsContainer,
    timeout: Duration,
    public_only: bool,
    module_cache: modules::ModuleCache,
}

#[derive(Debug, Serialize)]
pub struct ScriptResult {
    pub output: Option<String>,
    pub logs: Vec<String>,
    pub error: Option<String>,
    pub duration_ms: u128,
}

impl ScriptResult {
    fn error(message: impl Into<String>) -> Self {
        Self {
            output: None,
            logs: vec![],
            error: Some(message.into()),
            duration_ms: 0,
        }
    }
}

impl ScriptExecutor {
    pub fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        let allow_net = match std::env::var("PASTEBIN_JS_ALLOW_NET") {
            Ok(value) => {
                let hosts: Vec<String> = value
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect();
                if hosts.is_empty() {
                    return Err("PASTEBIN_JS_ALLOW_NET must contain at least one host".into());
                }
                hosts
            }
            Err(std::env::VarError::NotPresent) => vec![],
            Err(error) => return Err(error.into()),
        };
        Self::new(allow_net, true, TIMEOUT)
    }

    fn new(
        allow_net: Vec<String>,
        public_only: bool,
        timeout: Duration,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize V8 once, before execution can occur concurrently.
        JsRuntime::init_platform(None);
        let _ =
            deno_runtime::deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let parser = Arc::new(RuntimePermissionDescriptorParser::new(
            sys_traits::impls::RealSys,
        ));
        let mut permissions = Permissions::from_options(
            parser.as_ref(),
            &PermissionsOptions {
                allow_net: Some(allow_net.clone()),
                prompt: false,
                ..Default::default()
            },
        )?;
        if public_only {
            // Deno's string parser accepts IPv4 CIDRs but not IPv6 CIDRs;
            // construct subnet descriptors directly for both address families.
            permissions.net = Permissions::new_unary(
                Some(
                    allow_net
                        .iter()
                        .map(|host| parser.parse_net_descriptor(host))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Some(
                    BLOCKED_NETS
                        .iter()
                        .map(|range| NetDescriptor(Host::IpSubnet(range.parse().unwrap()), None))
                        .collect(),
                ),
                false,
            );
        }
        Ok(Self {
            slots: Arc::new(Semaphore::new(2)),
            permissions: PermissionsContainer::new(parser, permissions),
            timeout,
            public_only,
            module_cache: Default::default(),
        })
    }

    pub async fn execute(&self, source: String) -> ScriptResult {
        self.execute_inner(source, false).await
    }

    pub async fn execute_scheduled(&self, source: String) -> ScriptResult {
        self.execute_inner(source, true).await
    }

    async fn execute_inner(&self, source: String, wait: bool) -> ScriptResult {
        if source.trim().is_empty() {
            return ScriptResult::error("JavaScript source is required");
        }
        if source.len() > MAX_SOURCE {
            return ScriptResult::error("JavaScript source exceeds 256 KiB");
        }
        let permit = if wait {
            match tokio::time::timeout(Duration::from_secs(40), self.slots.clone().acquire_owned())
                .await
            {
                Ok(Ok(permit)) => permit,
                _ => {
                    return ScriptResult::error(
                        "Script executor is busy; scheduled run could not start",
                    );
                }
            }
        } else {
            match self.slots.clone().try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => return ScriptResult::error("Script executor is busy; try again shortly"),
            }
        };
        let executor = self.clone();
        // The permit belongs to the execution, even if the HTTP client disconnects.
        match tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let started = Instant::now();
            let mut result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(executor.run(source)),
                Err(error) => ScriptResult::error(error.to_string()),
            };
            result.duration_ms = started.elapsed().as_millis();
            result
        })
        .await
        {
            Ok(result) => result,
            Err(error) => {
                tracing::error!("script worker failed: {error}");
                ScriptResult::error("Script worker failed")
            }
        }
    }

    async fn run(&self, source: String) -> ScriptResult {
        let loader = match modules::HttpsModuleLoader::new(
            self.permissions.deep_clone(),
            self.public_only,
            self.module_cache.clone(),
        ) {
            Ok(loader) => loader,
            Err(error) => return ScriptResult::error(error.to_string()),
        };
        let services = WorkerServiceOptions::<
            deno_resolver::npm::DenoInNpmPackageChecker,
            deno_resolver::npm::NpmResolver<sys_traits::impls::RealSys>,
            sys_traits::impls::RealSys,
        > {
            blob_store: Default::default(),
            broadcast_channel: Default::default(),
            deno_rt_native_addon_loader: None,
            feature_checker: Default::default(),
            fs: Arc::new(deno_runtime::deno_fs::RealFs),
            module_loader: Rc::new(loader),
            node_services: None,
            npm_process_state_provider: None,
            permissions: self.permissions.deep_clone(),
            root_cert_store_provider: None,
            fetch_dns_resolver: if self.public_only {
                deno_runtime::deno_fetch::dns::Resolver::Custom(Arc::new(PublicResolver))
            } else {
                Default::default()
            },
            shared_array_buffer_store: None,
            compiled_wasm_module_store: None,
            v8_code_cache: None,
            bundle_provider: None,
        };
        let mut worker = MainWorker::bootstrap_from_options(
            &ModuleSpecifier::parse("file:///paste.js").unwrap(),
            services,
            WorkerOptions {
                extensions: vec![paste_console::init()],
                create_params: Some(v8::CreateParams::default().heap_limits(0, 64 * 1024 * 1024)),
                ..Default::default()
            },
        );
        let handle = worker.js_runtime.v8_isolate().thread_safe_handle();
        let memory_handle = handle.clone();
        worker
            .js_runtime
            .add_near_heap_limit_callback(move |current, _| {
                memory_handle.terminate_execution();
                // Give V8 room to deliver termination instead of immediately aborting.
                current + 16 * 1024 * 1024
            });
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let timeout = self.timeout;
        // A separate OS thread is essential: a JS loop can block Tokio polling.
        let watchdog = std::thread::spawn(move || {
            if matches!(
                done_rx.recv_timeout(timeout),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ) {
                handle.terminate_execution();
            }
        });
        let result = tokio::time::timeout(timeout, async {
            worker
                .js_runtime
                .execute_script("paste-bootstrap.js", include_str!("script_bootstrap.js"))
                .map_err(|e| e.to_string())?;
            let module = worker
                .js_runtime
                .load_main_es_module_from_code(&ModuleSpecifier::parse("file:///paste.js").unwrap(), source)
                .await
                .map_err(|e| e.to_string())?;
            let evaluation = worker.js_runtime.mod_evaluate(module);
            worker.js_runtime.with_event_loop_promise(evaluation, Default::default())
                .await.map_err(|e| e.to_string())?;
            let namespace = worker.js_runtime.get_module_namespace(module).map_err(|e| e.to_string())?;
            let function = {
                deno_core::scope!(scope, worker.js_runtime);
                let namespace = v8::Local::new(scope, namespace);
                let key = v8::String::new(scope, "default").unwrap();
                let value = namespace.get(scope, key.into())
                    .ok_or("Could not read the default export")?;
                let function = v8::Local::<v8::Function>::try_from(value)
                    .map_err(|_| "Export a default function: export default async function () { return \"content\"; }")?;
                v8::Global::new(scope, function)
            };
            let call = worker.js_runtime.call(&function);
            let value = worker
                .js_runtime
                .with_event_loop_promise(call, Default::default())
                .await
                .map_err(|e| e.to_string())?;
            deno_core::scope!(scope, worker.js_runtime);
            let value = v8::Local::new(scope, value);
            if !value.is_string() {
                return Err("Script must return a string (use JSON.stringify for JSON)".to_string());
            }
            let value = v8::Local::<v8::String>::try_from(value).unwrap();
            if value.utf8_length(scope) > MAX_OUTPUT {
                return Err("Output exceeds 1 MiB".to_string());
            }
            Ok(value.to_rust_string_lossy(scope))
        })
        .await;
        let _ = done_tx.send(());
        let _ = watchdog.join();
        let logs = std::mem::take(
            &mut worker
                .js_runtime
                .op_state()
                .borrow_mut()
                .borrow_mut::<Logs>()
                .lines,
        );
        match result {
            Ok(Ok(output)) => ScriptResult {
                output: Some(output),
                logs,
                error: None,
                duration_ms: 0,
            },
            other => {
                let message = match other {
                    Ok(Err(error)) => error,
                    Err(_) => "Script timed out".to_string(),
                    _ => unreachable!(),
                };
                ScriptResult {
                    output: None,
                    logs,
                    error: Some(message),
                    duration_ms: 0,
                }
            }
        }
    }
}

#[derive(Default)]
struct Logs {
    lines: Vec<String>,
    bytes: usize,
}

#[op2(fast)]
fn op_paste_log(state: &mut OpState, #[string] message: String) {
    let logs = state.borrow_mut::<Logs>();
    if logs.bytes >= MAX_LOGS {
        return;
    }
    let mut end = message.len().min(MAX_LOGS - logs.bytes);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    logs.lines.push(message[..end].to_string());
    logs.bytes += end + 1;
}

deno_core::extension!(paste_console, ops = [op_paste_log], esm_entry_point = "ext:paste_console/script_ops.js", esm = [dir "src", "script_ops.js"], state = |state| { state.put(Logs::default()); state.put(deno_runtime::ops::bootstrap::SnapshotOptions::default()); });

#[derive(Debug)]
struct PublicResolver;

fn public_ip(ip: IpAddr) -> bool {
    !BLOCKED_NETS
        .iter()
        .any(|range| range.parse::<ipnet::IpNet>().unwrap().contains(&ip))
}

impl deno_runtime::deno_fetch::dns::Resolve for PublicResolver {
    fn resolve(
        &self,
        name: hyper_util::client::legacy::connect::dns::Name,
    ) -> deno_runtime::deno_fetch::dns::Resolving {
        let name = name.as_str().to_owned();
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            if addresses.is_empty() || addresses.iter().any(|addr| !public_ip(addr.ip())) {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "Private and reserved network addresses are not allowed",
                ));
            }
            Ok(addresses.into_iter())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ScriptExecutor {
        async fn execute_body(&self, body: String) -> ScriptResult {
            self.execute(format!("export default async function () {{\n{body}\n}}"))
                .await
        }
    }

    // Optional smoke test against a real, separately downloaded ESM build.
    // PASTEBIN_TEST_YAML_MODULE=/path/to/js-yaml.mjs cargo test yaml_module_smoke -- --ignored
    #[tokio::test]
    #[ignore = "requires the js-yaml@4.1.1 ESM fixture path in PASTEBIN_TEST_YAML_MODULE"]
    async fn yaml_module_smoke() {
        let path = std::env::var("PASTEBIN_TEST_YAML_MODULE").expect("set the YAML fixture path");
        let yaml = std::fs::read_to_string(path).unwrap();
        let app = axum::Router::new().route(
            "/yaml.mjs",
            axum::routing::get(move || {
                let yaml = yaml.clone();
                async move {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/javascript")],
                        yaml,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(3)).unwrap();
        let source = format!(
            "import {{load, dump}} from 'http://{address}/yaml.mjs'; export default function () {{ const config = load('name: demo\\ninterval: 5\\n'); config.interval = 10; return dump(config); }}"
        );
        let result = executor.execute(source.clone()).await;
        assert_eq!(result.error, None, "{result:?}");
        assert_eq!(result.output.as_deref(), Some("name: demo\ninterval: 10\n"));
        server.abort();
        let cached = executor.execute(source).await;
        assert_eq!(cached.error, None, "{cached:?}");
        assert_eq!(cached.output, result.output);
    }

    #[tokio::test]
    async fn esm_entrypoint_and_remote_modules() {
        use axum::{
            Router,
            http::{StatusCode, header},
            response::Redirect,
            routing::get,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(3)).unwrap();
        for source in [
            "export default function () { return 'ok'; }",
            "export default async () => { await new Promise(r => setTimeout(r, 5)); return 'ok'; };",
            "await Promise.resolve(); function run() { return 'ok'; } export { run as default };",
        ] {
            let result = executor.execute(source.into()).await;
            assert_eq!(result.error, None, "{result:?}");
            assert_eq!(result.output.as_deref(), Some("ok"));
        }
        for source in ["export const value = 1;", "export default 'text';"] {
            let result = executor.execute(source.into()).await;
            assert!(result.error.unwrap().contains("default function"));
        }
        assert!(
            executor
                .execute("return 'legacy';".into())
                .await
                .error
                .is_some()
        );

        let hits = Arc::new(AtomicUsize::new(0));
        let requests = hits.clone();
        let failures = Arc::new(AtomicUsize::new(0));
        let attempts = failures.clone();
        let app = Router::new()
            .route("/entry.js", get(|| async { Redirect::temporary("/pkg/main.js") }))
            .route("/pkg/main.js", get(move || {
                requests.fetch_add(1, Ordering::SeqCst);
                async { ([(header::CONTENT_TYPE, "text/javascript")],
                    "import { value } from './nested.js'; globalThis.moduleRuns = (globalThis.moduleRuns || 0) + 1; export const read = () => value + ':' + globalThis.moduleRuns + ':' + typeof Deno;") }
            }))
            .route("/pkg/nested.js", get(|| async { ([(header::CONTENT_TYPE, "application/javascript")], "export const value = 'loaded';") }))
            .route("/html", get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<html>not JavaScript</html>") }))
            .route("/large.js", get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], "x".repeat(2 * 1024 * 1024 + 1)) }))
            .route("/retry.js", get(move || {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                async move { (if attempt == 0 { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK },
                    [(header::CONTENT_TYPE, "text/javascript")], "export const value = 'retried';") }
            }))
            .route("/loop.js", get(|| async { Redirect::temporary("/loop.js") }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let source = format!(
            "import {{read}} from 'http://{address}/entry.js'; export default async function () {{ const again = await import('http://{address}/entry.js'); return read() + ':' + (read === again.read); }}"
        );
        for _ in 0..2 {
            let result = executor.execute(source.clone()).await;
            assert_eq!(result.error, None, "{result:?}");
            assert_eq!(result.output.as_deref(), Some("loaded:1:undefined:true"));
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "Reuse downloaded source across executions"
        );
        for (path, message) in [
            ("html", "Content-Type"),
            ("large.js", "2 MiB"),
            ("loop.js", "5 redirects"),
            ("missing.js", "404"),
        ] {
            let result = executor
                .execute(format!(
                    "import 'http://{address}/{path}'; export default () => 'bad';"
                ))
                .await;
            assert!(result.error.unwrap().contains(message), "{path}");
        }
        let retry = format!(
            "import {{value}} from 'http://{address}/retry.js'; export default () => value;"
        );
        assert!(
            executor
                .execute(retry.clone())
                .await
                .error
                .unwrap()
                .contains("503")
        );
        assert_eq!(
            executor.execute(retry).await.output.as_deref(),
            Some("retried")
        );
        let restricted = ScriptExecutor::new(
            vec!["allowed.example".into()],
            false,
            Duration::from_secs(3),
        )
        .unwrap();
        assert!(restricted.execute(source.clone()).await.error.is_some());
        server.abort();
        assert_eq!(
            executor.execute(source).await.output.as_deref(),
            Some("loaded:1:undefined:true")
        );

        let public = ScriptExecutor::new(vec![], true, Duration::from_secs(3)).unwrap();
        for url in [
            "http://example.com/x.js",
            "https://127.0.0.1/x.js",
            "https://[::1]/x.js",
            "https://localhost/x.js",
            "file:///etc/passwd",
            "data:text/javascript,export default 1",
            "npm:yaml",
            "jsr:@std/yaml",
            "yaml",
            "https://user:pass@example.com/x.js",
        ] {
            let result = public
                .execute(format!("import '{url}'; export default () => 'bad';"))
                .await;
            assert!(result.error.is_some(), "must reject {url}");
        }
    }

    #[tokio::test]
    async fn execution_network_and_limits() {
        let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(2)).unwrap();
        let result = executor
            .execute_body("console.log('hello', {n: 1}); return `value:${[1, 2].at(-1)}`;".into())
            .await;
        assert_eq!(result.error, None, "{result:?}");
        assert_eq!(result.output.as_deref(), Some("value:2"));
        assert_eq!(result.logs, vec!["[log] hello {\"n\":1}"]);
        assert_eq!(
            executor
                .execute_body("return '';".into())
                .await
                .output
                .as_deref(),
            Some("")
        );
        assert!(
            executor
                .execute_body("return 42;".into())
                .await
                .error
                .unwrap()
                .contains("string")
        );
        assert!(
            executor
                .execute_body("return (;".into())
                .await
                .error
                .is_some()
        );
        let result = executor
            .execute_body("console.warn('before'); throw new Error('boom');".into())
            .await;
        assert!(result.error.unwrap().contains("boom"));
        assert_eq!(result.logs.len(), 1);
        let result = executor
            .execute_body("return [typeof Deno, typeof Worker, typeof navigator].join(',');".into())
            .await;
        assert_eq!(
            result.output.as_deref(),
            Some("undefined,undefined,undefined")
        );
        assert!(
            executor
                .execute_body("await import('file:///etc/passwd'); return 'bad';".into())
                .await
                .error
                .is_some()
        );
        assert!(
            executor
                .execute_body("return 'x'.repeat(1024 * 1024 + 1);".into())
                .await
                .error
                .unwrap()
                .contains("1 MiB")
        );
        executor
            .execute_body("globalThis.leak = 'secret'; return 'ok';".into())
            .await;
        assert_eq!(
            executor
                .execute_body("return typeof leak;".into())
                .await
                .output
                .as_deref(),
            Some("undefined")
        );

        let app = axum::Router::new().route(
            "/",
            axum::routing::post(|| async { axum::Json(serde_json::json!({"value":"network"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let source = format!(
            "const r = await fetch('http://{address}/', {{method: 'POST', body: 'hello'}}); return (await r.json()).value;"
        );
        assert_eq!(
            executor
                .execute_body(source.clone())
                .await
                .output
                .as_deref(),
            Some("network")
        );
        let public = ScriptExecutor::new(vec![], true, Duration::from_secs(2)).unwrap();
        assert!(public.execute_body(source).await.error.is_some());
        assert!(
            public
                .execute_body(format!(
                    "await fetch('http://localhost:{}/'); return 'bad';",
                    address.port()
                ))
                .await
                .error
                .is_some()
        );
        server.abort();

        let bounded = ScriptExecutor::new(vec![], false, Duration::from_millis(200)).unwrap();
        assert!(
            bounded
                .execute_body("while (true) {}".into())
                .await
                .error
                .is_some()
        );
        assert!(
            bounded
                .execute_body("await new Promise(r => setTimeout(r, 10000)); return 'late';".into())
                .await
                .error
                .is_some()
        );
        assert_eq!(
            bounded
                .execute_body("return 'recovered';".into())
                .await
                .output
                .as_deref(),
            Some("recovered")
        );
    }

    #[test]
    fn rejects_non_public_ip_ranges() {
        for address in [
            "127.0.0.1",
            "10.1.1.1",
            "169.254.169.254",
            "192.168.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
        ] {
            assert!(!public_ip(address.parse().unwrap()), "{address}");
        }
        assert!(public_ip("8.8.8.8".parse().unwrap()));
        assert!(public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
