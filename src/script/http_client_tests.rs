use super::*;
use std::io::{Read, Write};

const CA: &str = include_str!("../../tests/fixtures/mtls/ca.pem");
const CERT: &str = include_str!("../../tests/fixtures/mtls/client.pem");
const KEY: &str = include_str!("../../tests/fixtures/mtls/client-key.pem");

async fn run(executor: &ScriptExecutor, body: &str) -> ScriptResult {
    executor
        .execute(format!("export default async function () {{ {body} }}"))
        .await
}

#[tokio::test]
async fn http_client_mtls() {
    use deno_runtime::deno_tls::{load_certs, load_private_keys, rustls};
    let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(5)).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for cert in load_certs(&mut CA.as_bytes()).unwrap() {
        roots.add(cert).unwrap();
    }
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .unwrap();
    let config = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            load_certs(&mut include_bytes!("../../tests/fixtures/mtls/server.pem").as_slice())
                .unwrap(),
            load_private_keys(include_bytes!("../../tests/fixtures/mtls/server-key.pem"))
                .unwrap()
                .remove(0),
        )
        .unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = std::thread::spawn(move || {
        let config = Arc::new(config);
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut successes = 0;
        while Instant::now() < deadline {
            let (socket, _) = match listener.accept() {
                Ok(pair) => pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(error) => panic!("{error}"),
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let connection = rustls::ServerConnection::new(config.clone()).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, socket);
            let mut buffer = [0; 4096];
            if let Ok(size) = stream.read(&mut buffer) {
                if size > 0 {
                    assert!(!stream.conn.peer_certificates().unwrap().is_empty());
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nmtls").unwrap();
                    stream.conn.send_close_notify();
                    stream.flush().unwrap();
                    successes += 1;
                    break;
                }
            }
        }
        successes
    });
    let options = serde_json::json!({"caCerts": [CA]});
    let no_cert = run(&executor, &format!(
        "const client = Deno.createHttpClient({options}); try {{ return await (await fetch('https://{address}', {{client}})).text(); }} finally {{ client.close(); }}"
    )).await;
    assert!(no_cert.error.is_some(), "{no_cert:?}");
    let options = serde_json::json!({"cert": CERT, "key": KEY, "caCerts": [CA], "http1": true, "http2": false});
    let result = run(&executor, &format!(
        "const client = Deno.createHttpClient({options}); try {{ return await (await fetch('https://{address}', {{client}})).text(); }} finally {{ client.close(); }}"
    )).await;
    assert_eq!(result.error, None, "{result:?}");
    assert_eq!(result.output.as_deref(), Some("mtls"));
    assert_eq!(server.join().unwrap(), 1);
}

#[tokio::test]
async fn http_client_restrictions() {
    let executor = ScriptExecutor::new(vec![], true, Duration::from_secs(3)).unwrap();
    let surface = run(
        &executor,
        "return Object.keys(Deno).join(',') + ':' + typeof Deno.readFile;",
    )
    .await;
    assert_eq!(
        surface.output.as_deref(),
        Some("createHttpClient:undefined")
    );
    for options in [
        "{proxy: {url: 'http://localhost:8080'}}",
        "{certFile: '/etc/passwd'}",
        "{unsafelyIgnoreCertificateErrors: true}",
        "{keyFormat:'pem'}",
        "{http1: false, http2: false}",
        "{http1: 1}",
        "{http2: 'false'}",
        "{cert: 'bad'}",
        "{key: 'bad'}",
        "{cert:'bad',key:'bad'}",
        "{caCerts:[42]}",
        "{caCerts:['bad']}",
        "null",
    ] {
        assert!(
            run(
                &executor,
                &format!("Deno.createHttpClient({options}); return 'bad';")
            )
            .await
            .error
            .is_some(),
            "{options}"
        );
    }
    for url in ["http://127.0.0.1:1", "http://localhost:1"] {
        let result = run(&executor, &format!("const client = Deno.createHttpClient({{}}); try {{ await fetch('{url}', {{client}}); return 'bad'; }} finally {{ client.close(); }}")).await;
        let error = result.error.unwrap();
        assert!(
            error.contains("Requires net access") || error.contains("Private and reserved"),
            "{error}"
        );
    }
    let allowlist =
        ScriptExecutor::new(vec!["example.com".into()], false, Duration::from_secs(3)).unwrap();
    let result = run(&allowlist, "const client = Deno.createHttpClient({}); await fetch('http://127.0.0.1:1', {client}); return 'bad';").await;
    assert!(result.error.unwrap().contains("Requires net access"));
    assert!(
        run(
            &executor,
            "for(let i=0;i<11;i++) Deno.createHttpClient({}); return 'bad';"
        )
        .await
        .error
        .unwrap()
        .contains("At most 10 HTTP clients")
    );
    assert!(
        run(
            &executor,
            "await fetch('https://example.com', {client: {}}); return 'bad';"
        )
        .await
        .error
        .unwrap()
        .contains("client must be created")
    );
}

#[tokio::test]
async fn http_client_fetch_limits_and_close() {
    let app = axum::Router::new()
        .route("/", axum::routing::get(|| async { "ok" }))
        .route(
            "/large",
            axum::routing::get(|| async { vec![b'x'; 5 * 1024 * 1024 + 1] }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let executor = ScriptExecutor::new(vec![], false, Duration::from_secs(5)).unwrap();
    for (body, message) in [
        (
            format!(
                "const client = Deno.createHttpClient({{}}); client.close(); await fetch('http://{address}', {{client}}); return 'bad';"
            ),
            "Bad resource",
        ),
        (
            format!(
                "const client = Deno.createHttpClient({{}}); for(let i=0;i<11;i++) await fetch('http://{address}', {{client}}); return 'bad';"
            ),
            "At most 10 fetch",
        ),
        (
            format!(
                "const client = Deno.createHttpClient({{}}); await fetch('http://{address}/large', {{client}}); return 'bad';"
            ),
            "Response exceeds 5 MiB",
        ),
    ] {
        let result = run(&executor, &body).await;
        assert!(
            result.error.as_deref().unwrap_or("").contains(message),
            "{result:?}"
        );
    }
    server.abort();
}
