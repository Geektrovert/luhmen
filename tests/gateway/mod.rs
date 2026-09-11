use super::*;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener as StdListener, TcpStream as StdStream};
use std::thread::{self, JoinHandle};
use std::time::Instant;

struct Gateway {
    directory: tempfile::TempDir,
    port: u16,
    shutdown: Cancellation,
    thread: Option<JoinHandle<Result<()>>>,
}

impl Gateway {
    fn new(upstream_port: u16) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let port = StdListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config_path = directory.path().join("gateway.json");
        fs::write(
            &config_path,
            serde_json::json!({
                "listen_port": port,
                "routes": {"app.localhost": upstream_port, "other.localhost": upstream_port}
            })
            .to_string(),
        )
        .unwrap();
        init(directory.path()).unwrap();
        let mut gateway = Self {
            directory,
            port,
            shutdown: Cancellation::new(),
            thread: None,
        };
        gateway.start();
        gateway
    }

    fn start(&mut self) {
        self.shutdown = Cancellation::new();
        let state_dir = self.directory.path().to_owned();
        let config_path = state_dir.join("gateway.json");
        let shutdown = self.shutdown.clone();
        self.thread = Some(thread::spawn(move || {
            run(&state_dir, &config_path, shutdown)
        }));
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if StdStream::connect((Ipv4Addr::LOCALHOST, self.port)).is_ok() {
                return;
            }
            if self.thread.as_ref().unwrap().is_finished() {
                panic!(
                    "gateway exited before readiness: {:?}",
                    self.thread.take().unwrap().join()
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("gateway did not accept connections within five seconds");
    }

    fn stop(&mut self) {
        self.shutdown.cancel();
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap().unwrap();
        }
    }

    fn tls(
        &self,
        hostname: &'static str,
    ) -> rustls::StreamOwned<rustls::ClientConnection, StdStream> {
        let connection = rustls::ClientConnection::new(
            client_config(self.directory.path()),
            hostname.try_into().unwrap(),
        )
        .unwrap();
        let stream = StdStream::connect((Ipv4Addr::LOCALHOST, self.port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        rustls::StreamOwned::new(connection, stream)
    }

    fn request(&self, hostname: &'static str, request: &str) -> String {
        let mut stream = self.tls(hostname);
        stream.write_all(request.as_bytes()).unwrap();
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .unwrap_or_else(|error| {
                panic!("TLS response read failed: {error}; response: {response:?}")
            });
        response
    }
}

fn client_config(state_dir: &Path) -> Arc<rustls::ClientConfig> {
    let pem = fs::read(state_dir.join("gateway-ca/ca.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for certificate in rustls_pemfile::certs(&mut pem.as_slice()) {
        roots.add(certificate.unwrap()).unwrap();
    }
    Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth(),
    )
}

impl Drop for Gateway {
    fn drop(&mut self) {
        self.stop();
    }
}

fn upstream(response: impl AsRef<[u8]>) -> (u16, JoinHandle<String>) {
    let response = response.as_ref().to_vec();
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let thread = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut stream = loop {
            if let Ok((stream, _)) = listener.accept() {
                break stream;
            }
            assert!(Instant::now() < deadline, "upstream received no connection");
            thread::sleep(Duration::from_millis(5));
        };
        stream.set_nonblocking(false).unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut received = Vec::new();
        let mut byte = [0; 1];
        while !received.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).unwrap();
            received.push(byte[0]);
            assert!(received.len() < 32768);
        }
        let header = String::from_utf8(received.clone()).unwrap();
        let length: usize = header
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).unwrap();
        received.extend(body);
        stream.write_all(&response).unwrap();
        String::from_utf8(received).unwrap()
    });
    (port, thread)
}

#[test]
fn https_certificate_passes_openssl_strict_verification() {
    let reserved_upstream = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let gateway = Gateway::new(reserved_upstream.local_addr().unwrap().port());
    let result = std::process::Command::new("python3")
        .args([
            "-c",
            r#"
import socket, ssl, sys
context = ssl.create_default_context(cafile=sys.argv[1])
context.verify_flags |= ssl.VERIFY_X509_STRICT
with socket.create_connection(("127.0.0.1", int(sys.argv[2])), timeout=5) as stream:
    with context.wrap_socket(stream, server_hostname="app.localhost") as tls:
        assert tls.getpeercert()["subjectAltName"] == (("DNS", "app.localhost"),)
"#,
        ])
        .arg(gateway.directory.path().join("gateway-ca/ca.pem"))
        .arg(gateway.port.to_string())
        .output()
        .expect("Python 3 with OpenSSL is required for the TLS interoperability test");
    assert!(
        result.status.success(),
        "strict TLS verification failed: {}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn https_forwards_chunked_bodies_and_filters_connection_headers() {
    let (upstream_port, upstream) = upstream(
        "HTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\nConnection: close, x-private\r\nX-Private: secret\r\nX-App: present\r\n\r\n5\r\nhello\r\n6\r\n world\r\n0\r\n\r\n",
    );
    let gateway = Gateway::new(upstream_port);
    let reply = gateway.request("app.localhost", &format!(
        "POST /echo?x=1 HTTP/1.1\r\nHost: app.localhost:{}\r\nTransfer-Encoding: chunked\r\nConnection: close, x-hop\r\nX-Hop: secret\r\nX-Forwarded-Proto: forged\r\nX-Forwarded-For: forged\r\nAuthorization: Bearer fixture\r\n\r\n3\r\none\r\n3\r\ntwo\r\n0\r\n\r\n", gateway.port));
    assert!(reply.starts_with("HTTP/1.1 201"), "{reply}");
    assert!(reply.ends_with("hello world"), "{reply}");
    assert!(reply.to_ascii_lowercase().contains("x-app: present"));
    assert!(!reply.to_ascii_lowercase().contains("x-private"));
    let forwarded = upstream.join().unwrap().to_ascii_lowercase();
    assert!(
        forwarded.starts_with("post /echo?x=1 http/1.1"),
        "{forwarded}"
    );
    assert!(forwarded.ends_with("onetwo"));
    assert!(forwarded.contains("authorization: bearer fixture"));
    assert!(forwarded.contains("x-forwarded-proto: https"));
    assert!(forwarded.contains("x-forwarded-for: 127.0.0.1"));
    assert!(!forwarded.contains("x-hop") && !forwarded.contains("forged"));
    assert!(!forwarded.contains("transfer-encoding"));
}

#[test]
fn gateway_rejects_unknown_sni_mismatched_hosts_and_proxy_requests() {
    let reserved_upstream = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let gateway = Gateway::new(reserved_upstream.local_addr().unwrap().port());
    let mut unknown = gateway.tls("missing.localhost");
    let handshake = unknown.conn.complete_io(&mut unknown.sock);
    assert!(handshake.is_err(), "unconfigured SNI must fail TLS");
    let reply = gateway.request(
        "app.localhost",
        &format!(
            "GET / HTTP/1.1\r\nHost: other.localhost:{}\r\n\r\n",
            gateway.port
        ),
    );
    assert!(reply.starts_with("HTTP/1.1 421"), "{reply}");
    let reply = gateway.request(
        "app.localhost",
        &format!(
            "GET http://example.com/ HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
            gateway.port
        ),
    );
    assert!(reply.starts_with("HTTP/1.1 400"), "{reply}");
    let reply = gateway.request("app.localhost", &format!("GET / HTTP/1.1\r\nHost: app.localhost:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n", gateway.port));
    assert!(reply.starts_with("HTTP/1.1 501"), "{reply}");
    reserved_upstream.set_nonblocking(true).unwrap();
    assert!(
        reserved_upstream.accept().is_err(),
        "rejected requests must not reach upstream"
    );
}

#[test]
fn restart_preserves_ca_and_reports_unavailable_upstreams() {
    let unused_port = StdListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut gateway = Gateway::new(unused_port);
    let ca_path = init(gateway.directory.path()).unwrap();
    let ca = fs::read(&ca_path).unwrap();
    let request = format!(
        "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
        gateway.port
    );
    assert!(
        gateway
            .request("app.localhost", &request)
            .starts_with("HTTP/1.1 502")
    );
    gateway.stop();
    let bound = StdListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, gateway.port))).unwrap();
    drop(bound);
    gateway.start();
    assert_eq!(fs::read(ca_path).unwrap(), ca);
    assert!(
        gateway
            .request("app.localhost", &request)
            .starts_with("HTTP/1.1 502")
    );
}

#[test]
fn ca_initialization_is_idempotent_and_refuses_corrupt_identity() {
    let directory = tempfile::tempdir().unwrap();
    let path = init(directory.path()).unwrap();
    let original = fs::read(&path).unwrap();
    assert_eq!(init(directory.path()).unwrap(), path);
    assert_eq!(fs::read(&path).unwrap(), original);
    let key_path = directory.path().join("gateway-ca/key.pem");
    assert_eq!(
        fs::metadata(&key_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    fs::write(&key_path, "corrupt").unwrap();
    assert!(init(directory.path()).is_err());
    assert_eq!(fs::read(path).unwrap(), original);
    assert_eq!(fs::read_to_string(key_path).unwrap(), "corrupt");
}

#[test]
fn config_rejects_external_domains_invalid_names_and_unknown_fields() {
    for host in [
        "example.com",
        "localhost",
        "*.localhost",
        "a..localhost",
        "-a.localhost",
        "a-.localhost",
        "APP.localhost",
        "a.localhost.",
    ] {
        assert!(!valid_hostname(host), "{host}");
    }
    for host in ["app.localhost", "a-b.localhost", "api.app.localhost"] {
        assert!(valid_hostname(host), "{host}");
    }
    let config: Config =
        serde_json::from_str(r#"{"listen_port":443,"routes":{"app.localhost":8080}}"#).unwrap();
    assert!(config.validate().is_err());
    let config: Config = serde_json::from_str(r#"{"routes":{"app.localhost":8443}}"#).unwrap();
    assert!(config.validate().is_err());
    assert!(
        serde_json::from_str::<Config>(
            r#"{"routes":{"app.localhost":8080},"listen_host":"0.0.0.0"}"#
        )
        .is_err()
    );
}

#[test]
fn oversized_bodies_are_rejected_without_unbounded_buffering() {
    let reserved_upstream = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let gateway = Gateway::new(reserved_upstream.local_addr().unwrap().port());
    let body = "x".repeat(MAX_BODY_BYTES + 1);
    let request = format!(
        "POST / HTTP/1.1\r\nHost: app.localhost:{}\r\nContent-Length: {}\r\n\r\n{}",
        gateway.port,
        body.len(),
        body
    );
    assert!(
        gateway
            .request("app.localhost", &request)
            .starts_with("HTTP/1.1 413")
    );
    reserved_upstream.set_nonblocking(true).unwrap();
    assert!(reserved_upstream.accept().is_err());
    let (port, upstream) = upstream(format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    ));
    let gateway = Gateway::new(port);
    assert!(
        gateway
            .request(
                "app.localhost",
                &format!(
                    "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
                    gateway.port
                )
            )
            .starts_with("HTTP/1.1 502")
    );
    upstream.join().unwrap();
}

#[test]
fn one_daemon_owns_a_state_directory_and_shutdown_releases_the_lock() {
    let upstream = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let mut gateway = Gateway::new(upstream.local_addr().unwrap().port());
    let error = run(
        gateway.directory.path(),
        &gateway.directory.path().join("gateway.json"),
        Cancellation::new(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("another luhmen gateway"));
    gateway.stop();
    gateway.start();
}

#[test]
fn slow_uploads_reject_overload_and_release_capacity_on_completion() {
    let (port, upstream) = upstream("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
    let gateway = Gateway::new(port);
    let mut uploads = Vec::new();
    for _ in 0..MAX_BUFFERED_EXCHANGES {
        let mut stream = gateway.tls("app.localhost");
        write!(
            stream,
            "POST / HTTP/1.1\r\nHost: app.localhost:{}\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n",
            gateway.port
        )
        .unwrap();
        // Continue is sent only after the handler starts reading the body.
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            headers.push(byte[0]);
            assert!(headers.len() < 1024);
        }
        assert!(headers.starts_with(b"HTTP/1.1 100"));
        uploads.push(stream);
    }
    let request = format!(
        "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
        gateway.port
    );
    let rejected = gateway.request("app.localhost", &request);
    assert!(rejected.starts_with("HTTP/1.1 503"), "{rejected}");

    let mut completed = uploads.pop().unwrap();
    completed.write_all(b"x").unwrap();
    let mut reply = String::new();
    completed.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    assert!(upstream.join().unwrap().ends_with('x'));

    // The upstream is now gone. A 502 proves the next request was admitted and
    // attempted a connection, rather than remaining stuck at the buffer limit.
    let admitted = gateway.request("app.localhost", &request);
    assert!(admitted.starts_with("HTTP/1.1 502"), "{admitted}");
    drop(uploads);
}

#[tokio::test]
async fn slow_response_writes_hold_reservations_until_disconnect() {
    use tokio::io::AsyncReadExt;

    let (port, upstream) = upstream(format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
        MAX_BODY_BYTES,
        "x".repeat(MAX_BODY_BYTES)
    ));
    let directory = tempfile::tempdir().unwrap();
    let config = Arc::new(Config {
        listen_port: 8443,
        routes: BTreeMap::from([("app.localhost".to_owned(), port)]),
    });
    let acceptor = TlsAcceptor::from(Arc::new(
        tls_config(directory.path(), &config.routes).unwrap(),
    ));
    let connector = tokio_rustls::TlsConnector::from(client_config(directory.path()));
    let buffers = Arc::new(Semaphore::new(1));
    // A bounded transport gives deterministic write backpressure. The same TLS
    // handshake, HTTP connection, forwarding, and budget code run for TCP.
    let (client, server) = tokio::io::duplex(1024);
    let server = tokio::spawn(serve_connection(
        server,
        acceptor.clone(),
        config.clone(),
        buffers.clone(),
    ));
    let mut slow = connector
        .connect("app.localhost".try_into().unwrap(), client)
        .await
        .unwrap();
    let request = b"GET / HTTP/1.1\r\nHost: app.localhost:8443\r\n\r\n";
    slow.write_all(request).await.unwrap();
    timeout(Duration::from_secs(5), slow.read_exact(&mut [0]))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(buffers.available_permits(), 0);
    assert!(!server.is_finished());

    let (client, extra_server) = tokio::io::duplex(1024);
    let extra_server = tokio::spawn(serve_connection(
        extra_server,
        acceptor,
        config,
        buffers.clone(),
    ));
    let mut extra = connector
        .connect("app.localhost".try_into().unwrap(), client)
        .await
        .unwrap();
    extra.write_all(request).await.unwrap();
    let mut reply = Vec::new();
    timeout(Duration::from_secs(5), extra.read_to_end(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert!(reply.starts_with(b"HTTP/1.1 503"));
    extra_server.await.unwrap();
    assert_eq!(buffers.available_permits(), 0);

    drop(slow);
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    let released = timeout(Duration::from_secs(5), buffers.acquire())
        .await
        .unwrap()
        .unwrap();
    drop(released);
    assert_eq!(buffers.available_permits(), 1);
    upstream.join().unwrap();
}
