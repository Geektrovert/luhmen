use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener as StdListener, TcpStream as StdStream};
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
        read_http_response(&mut stream)
            .unwrap_or_else(|error| panic!("TLS response read failed: {error}"))
    }
}

fn client_config(state_dir: &Path) -> Arc<rustls::ClientConfig> {
    let pem = fs::read(state_dir.join("gateway-ca/ca.pem")).unwrap();
    let mut roots = rustls::RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(&pem) {
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
        let header = read_http_headers(&mut stream).unwrap();
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
        let mut received = header.into_bytes();
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
    let result = std::process::Command::new("openssl")
        .args([
            "s_client",
            "-verify_return_error",
            "-x509_strict",
            "-verify_hostname",
            "app.localhost",
            "-servername",
            "app.localhost",
            "-CAfile",
        ])
        .arg(gateway.directory.path().join("gateway-ca/ca.pem"))
        .args(["-connect", &format!("127.0.0.1:{}", gateway.port)])
        .stdin(std::process::Stdio::null())
        .output()
        .expect("OpenSSL is required for the TLS interoperability test");
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
    let reply = gateway.request(
        "app.localhost",
        &format!(
            "OPTIONS * HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
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
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
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
    for _ in 0..MAX_EXCHANGES {
        let mut stream = gateway.tls("app.localhost");
        write!(
            stream,
            "POST / HTTP/1.1\r\nHost: app.localhost:{}\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n",
            gateway.port
        )
        .unwrap();
        // Continue is sent only after the handler starts reading the body.
        let headers = read_http_headers(&mut stream).unwrap();
        assert!(headers.len() < 1024);
        assert!(headers.starts_with("HTTP/1.1 100"));
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
    let reply = read_http_response(&mut completed).unwrap();
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

    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_http_headers(&mut stream).unwrap();
        stream.write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{}",
                MAX_BODY_BYTES,
                "x".repeat(MAX_BODY_BYTES)
            )
            .as_bytes(),
        )
    });
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
        upstream_client(),
        Cancellation::new(),
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
        upstream_client(),
        Cancellation::new(),
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
    assert!(
        upstream.join().unwrap().is_err(),
        "slow client must backpressure the upstream before all 8 MiB are read"
    );
}

fn read_http_headers(stream: &mut impl Read) -> std::io::Result<String> {
    let mut headers = Vec::new();
    while !headers.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        headers.push(byte[0]);
        assert!(headers.len() < 32768);
    }
    Ok(String::from_utf8(headers).unwrap())
}

fn read_http_response(stream: &mut impl Read) -> std::io::Result<String> {
    let headers = read_http_headers(stream)?;
    let mut body = Vec::new();
    if headers
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        loop {
            let mut line = Vec::new();
            while !line.ends_with(b"\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte)?;
                line.push(byte[0]);
            }
            let length =
                usize::from_str_radix(std::str::from_utf8(&line).unwrap().trim(), 16).unwrap();
            if length == 0 {
                stream.read_exact(&mut [0; 2])?;
                break;
            }
            let start = body.len();
            body.resize(start + length, 0);
            stream.read_exact(&mut body[start..])?;
            stream.read_exact(&mut [0; 2])?;
        }
    } else if let Some(length) = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse::<usize>().unwrap())
    }) {
        body.resize(length, 0);
        stream.read_exact(&mut body)?;
    } else {
        stream.read_to_end(&mut body)?;
    }
    Ok(headers + std::str::from_utf8(&body).unwrap())
}

#[test]
fn https_streams_first_chunk_before_upstream_finishes() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, released) = std::sync::mpsc::channel();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_http_headers(&mut stream).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n")
            .unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
        let _ = stream.write_all(b"5\r\nworld\r\n0\r\n\r\n");
    });
    let gateway = Gateway::new(port);
    let mut stream = gateway.tls("app.localhost");
    stream
        .sock
        .set_read_timeout(Some(Duration::from_secs(1)))
        .unwrap();
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\nConnection: close\r\n\r\n",
        gateway.port
    )
    .unwrap();
    let mut received = Vec::new();
    let first_chunk = loop {
        let mut byte = [0];
        match stream.read_exact(&mut byte) {
            Ok(()) => received.push(byte[0]),
            Err(error) => break Err(error),
        }
        if received.ends_with(b"hello") {
            break Ok(());
        }
    };
    release.send(()).unwrap();
    upstream.join().unwrap();
    assert!(
        first_chunk.is_ok(),
        "first chunk was buffered until completion: {first_chunk:?}"
    );
    stream
        .sock
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.read_to_end(&mut received).unwrap();
    assert!(received.windows(5).any(|bytes| bytes == b"world"));
}

#[test]
fn https_reuses_client_and_upstream_connections() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        for _ in 0..3 {
            read_http_headers(&mut stream)?;
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")?;
        }
        Ok::<_, std::io::Error>(())
    });
    let gateway = Gateway::new(port);
    let mut stream = gateway.tls("app.localhost");
    for index in 0..2 {
        write!(
            stream,
            "GET /{index} HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
            gateway.port
        )
        .unwrap();
        let headers = read_http_headers(&mut stream).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
        assert!(
            !headers.to_ascii_lowercase().contains("connection: close"),
            "successful responses must keep the client connection alive: {headers}"
        );
        let mut body = [0; 2];
        stream.read_exact(&mut body).unwrap();
        assert_eq!(&body, b"OK");
    }
    drop(stream);
    let reply = gateway.request(
        "app.localhost",
        &format!(
            "GET /third HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
            gateway.port
        ),
    );
    assert!(reply.ends_with("OK"), "{reply}");
    assert!(
        upstream.join().unwrap().is_ok(),
        "requests from both TLS clients must reuse the same upstream connection"
    );
}

#[test]
fn streamed_body_limit_closes_incomplete_response() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_http_headers(&mut stream).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
            .unwrap();
        let chunk = format!("100000\r\n{}\r\n", "x".repeat(1024 * 1024));
        for _ in 0..9 {
            if stream.write_all(chunk.as_bytes()).is_err() {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n");
    });
    let gateway = Gateway::new(port);
    let mut stream = gateway.tls("app.localhost");
    write!(
        stream,
        "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
        gateway.port
    )
    .unwrap();
    let headers = read_http_headers(&mut stream).unwrap();
    assert!(headers.starts_with("HTTP/1.1 200"), "{headers}");
    let mut body = Vec::new();
    // A body error after headers must terminate HTTP and TLS, not send a second
    // status or a final chunk that would make the truncated response look valid.
    let _ = stream.read_to_end(&mut body);
    let payload_bytes = body.iter().filter(|byte| **byte == b'x').count();
    assert!(payload_bytes > 0 && payload_bytes <= MAX_BODY_BYTES);
    assert!(!body.ends_with(b"\r\n0\r\n\r\n"));
    upstream.join().unwrap();
}

#[test]
fn closing_upstreams_reconnect_and_idle_clients_shutdown_promptly() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let upstream = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            read_http_headers(&mut stream).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .unwrap();
        }
    });
    let mut gateway = Gateway::new(port);
    let mut stream = gateway.tls("app.localhost");
    for _ in 0..2 {
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: app.localhost:{}\r\n\r\n",
            gateway.port
        )
        .unwrap();
        assert!(read_http_response(&mut stream).unwrap().ends_with("OK"));
    }
    upstream.join().unwrap();
    let started = Instant::now();
    gateway.stop();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "idle keep-alive clients must not wait for the five-second drain deadline"
    );
    assert_eq!(stream.read(&mut [0]).unwrap(), 0);
}

#[tokio::test]
async fn streamed_response_deadline_cancels_a_stalled_upstream() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (release, released) = std::sync::mpsc::channel();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_http_headers(&mut stream).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n")
            .unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let capacity = Arc::new(Semaphore::new(1));
    let reservation = Arc::new(capacity.clone().acquire_owned().await.unwrap());
    let request = Request::builder()
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(GatewayBody::buffered(Bytes::new(), None))
        .unwrap();
    let response = exchange(
        &upstream_client(),
        request,
        false,
        reservation,
        tokio::time::Instant::now() + Duration::from_millis(500),
    )
    .await
    .unwrap();
    let mut body = response.into_body();
    assert_eq!(
        body.frame().await.unwrap().unwrap().into_data().unwrap(),
        "hello"
    );
    let error = timeout(Duration::from_secs(2), body.frame())
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    drop(body);
    assert_eq!(capacity.available_permits(), 1);
    release.send(()).unwrap();
    upstream.join().unwrap();
}

#[tokio::test]
async fn stalled_upstream_upload_keeps_its_buffer_reserved() {
    let listener = StdListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (ready, headers_received) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let upstream = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        read_http_headers(&mut stream).unwrap();
        ready.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(5)).unwrap();
    });
    let capacity = Arc::new(Semaphore::new(1));
    let reservation = Arc::new(capacity.clone().acquire_owned().await.unwrap());
    let request = Request::builder()
        .method(Method::POST)
        .uri(format!("http://127.0.0.1:{port}/"))
        .body(GatewayBody::buffered(
            Bytes::from(vec![b'x'; MAX_BODY_BYTES]),
            Some(reservation),
        ))
        .unwrap();
    let pending = tokio::spawn(upstream_client().request(request));
    timeout(Duration::from_secs(5), headers_received)
        .await
        .unwrap()
        .unwrap();
    // The upstream has read only headers. TCP cannot buffer all 8 MiB, so Hyper
    // still owns upload bytes even after the Body yielded its final frame.
    let reserved = capacity.available_permits();
    pending.abort();
    let _ = pending.await;
    release.send(()).unwrap();
    upstream.join().unwrap();
    assert_eq!(reserved, 0, "in-flight upload bytes lost their reservation");
    let _released = timeout(Duration::from_secs(2), capacity.acquire())
        .await
        .unwrap()
        .unwrap();
}
