//! Embedded Deno executor. Only administrators author scripts; public raw URLs
//! may execute them. Each invocation has its own isolate and bounded lifetime.
use deno_runtime::deno_core::{
    self, JsRuntime, ModuleSpecifier, NoopModuleLoader, OpState, op2, v8,
};
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
            module_loader: Rc::new(NoopModuleLoader),
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
            let program = format!("(async () => {{\n{source}\n}})()\n//# sourceURL=paste.js");
            let value = worker
                .js_runtime
                .execute_script("paste.js", program)
                .map_err(|e| e.to_string())?;
            #[allow(deprecated)]
            let value = worker
                .js_runtime
                .resolve_value(value)
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

    #[tokio::test]
    async fn execution_network_and_limits() {
        let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(2)).unwrap();
        let result = executor
            .execute("console.log('hello', {n: 1}); return `value:${[1, 2].at(-1)}`;".into())
            .await;
        assert_eq!(result.error, None, "{result:?}");
        assert_eq!(result.output.as_deref(), Some("value:2"));
        assert_eq!(result.logs, vec!["[log] hello {\"n\":1}"]);
        assert_eq!(
            executor
                .execute("return '';".into())
                .await
                .output
                .as_deref(),
            Some("")
        );
        assert!(
            executor
                .execute("return 42;".into())
                .await
                .error
                .unwrap()
                .contains("string")
        );
        assert!(executor.execute("return (;".into()).await.error.is_some());
        let result = executor
            .execute("console.warn('before'); throw new Error('boom');".into())
            .await;
        assert!(result.error.unwrap().contains("boom"));
        assert_eq!(result.logs.len(), 1);
        let result = executor
            .execute("return [typeof Deno, typeof Worker, typeof navigator].join(',');".into())
            .await;
        assert_eq!(
            result.output.as_deref(),
            Some("undefined,undefined,undefined")
        );
        assert!(
            executor
                .execute("await import('file:///etc/passwd'); return 'bad';".into())
                .await
                .error
                .is_some()
        );
        assert!(
            executor
                .execute("return 'x'.repeat(1024 * 1024 + 1);".into())
                .await
                .error
                .unwrap()
                .contains("1 MiB")
        );
        executor
            .execute("globalThis.leak = 'secret'; return 'ok';".into())
            .await;
        assert_eq!(
            executor
                .execute("return typeof leak;".into())
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
            executor.execute(source.clone()).await.output.as_deref(),
            Some("network")
        );
        let public = ScriptExecutor::new(vec![], true, Duration::from_secs(2)).unwrap();
        assert!(public.execute(source).await.error.is_some());
        assert!(
            public
                .execute(format!(
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
                .execute("while (true) {}".into())
                .await
                .error
                .is_some()
        );
        assert!(
            bounded
                .execute("await new Promise(r => setTimeout(r, 10000)); return 'late';".into())
                .await
                .error
                .is_some()
        );
        assert_eq!(
            bounded
                .execute("return 'recovered';".into())
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
