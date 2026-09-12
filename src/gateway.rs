//! A loopback HTTPS gateway for explicitly configured `.localhost` applications.

use crate::cancel::Cancellation;
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{CONNECTION, CONTENT_LENGTH, HOST, HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use hyper_util::rt::{TokioExecutor, TokioIo, TokioTimer};
use rcgen::{BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, GeneralSubtree};
use rcgen::{IsCa, Issuer, KeyPair, KeyUsagePurpose, NameConstraints};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};
use rustls::server::ResolvesServerCertUsingSni;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{self, File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};
use tokio::task::JoinSet;
use tokio::time::{Instant, Sleep, timeout};
use tokio_rustls::TlsAcceptor;

const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 32;
// Uploads are validated before forwarding and may briefly retain the old buffer
// during growth. Responses stream through bounded transport buffers.
const EXCHANGE_BUFFER_BYTES: usize = 2 * MAX_BODY_BYTES;
const BUFFER_BUDGET_BYTES: usize = 128 * 1024 * 1024;
const MAX_EXCHANGES: usize = BUFFER_BUDGET_BYTES / EXCHANGE_BUFFER_BYTES;
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

type HttpResponse = Response<GatewayBody>;
type Reservation = Arc<OwnedSemaphorePermit>;
type Upstream = Client<HttpConnector, GatewayBody>;

fn upstream_client() -> Upstream {
    let mut connector = HttpConnector::new();
    connector.set_connect_timeout(Some(HANDSHAKE_TIMEOUT));
    connector.set_nodelay(true);
    Client::builder(TokioExecutor::new())
        .pool_timer(TokioTimer::new())
        .pool_idle_timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(1)
        .http1_max_buf_size(32 * 1024)
        .http1_writev(true)
        .build(connector)
}

enum BodySource {
    Buffered(Full<Bytes>),
    Streaming(Incoming),
}

struct ReservedBytes {
    bytes: Bytes,
    _reservation: Reservation,
}

impl AsRef<[u8]> for ReservedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

struct GatewayBody {
    source: BodySource,
    remaining: usize,
    deadline: Option<Pin<Box<Sleep>>>,
    // Upstream request tasks can outlive a cancelled handler. Keep their upload
    // allocation reserved until Hyper drops it, as well as during response writes.
    _reservation: Option<Reservation>,
}

impl GatewayBody {
    fn buffered(bytes: Bytes, reservation: Option<Reservation>) -> Self {
        // Hyper may drop the Body before flushing its final frame. Tie the
        // reservation to the byte allocation so queued writes retain it too.
        let bytes = match &reservation {
            Some(reservation) => Bytes::from_owner(ReservedBytes {
                bytes,
                _reservation: reservation.clone(),
            }),
            None => bytes,
        };
        Self {
            source: BodySource::Buffered(Full::new(bytes)),
            remaining: MAX_BODY_BYTES,
            deadline: None,
            _reservation: reservation,
        }
    }

    fn streaming(body: Incoming, reservation: Reservation, deadline: Instant) -> Self {
        Self {
            source: BodySource::Streaming(body),
            remaining: MAX_BODY_BYTES,
            deadline: Some(Box::pin(tokio::time::sleep_until(deadline))),
            _reservation: Some(reservation),
        }
    }
}

impl Body for GatewayBody {
    type Data = Bytes;
    type Error = std::io::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if let Some(deadline) = &mut self.deadline
            && deadline.as_mut().poll(cx).is_ready()
        {
            return Poll::Ready(Some(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "gateway exchange exceeded 30 seconds",
            ))));
        }
        loop {
            let frame = match &mut self.source {
                BodySource::Buffered(body) => match Pin::new(body).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => frame,
                    Poll::Ready(Some(Err(never))) => match never {},
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                },
                BodySource::Streaming(body) => match Pin::new(body).poll_frame(cx) {
                    Poll::Ready(Some(Ok(frame))) => frame,
                    Poll::Ready(Some(Err(error))) => {
                        return Poll::Ready(Some(Err(std::io::Error::other(error))));
                    }
                    Poll::Ready(None) => return Poll::Ready(None),
                    Poll::Pending => return Poll::Pending,
                },
            };
            // Trailer forwarding remains disabled with the other hop headers.
            let Ok(bytes) = frame.into_data() else {
                continue;
            };
            if bytes.len() > self.remaining {
                return Poll::Ready(Some(Err(std::io::Error::other("body exceeds 8 MiB"))));
            }
            self.remaining -= bytes.len();
            return Poll::Ready(Some(Ok(Frame::data(bytes))));
        }
    }

    fn is_end_stream(&self) -> bool {
        match &self.source {
            BodySource::Buffered(body) => body.is_end_stream(),
            BodySource::Streaming(body) => body.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match &self.source {
            BodySource::Buffered(body) => body.size_hint(),
            BodySource::Streaming(body) => body.size_hint(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_port")]
    listen_port: u16,
    routes: BTreeMap<String, u16>,
}

fn default_port() -> u16 {
    8443
}

impl Config {
    fn read(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        File::open(path)
            .with_context(|| format!("open gateway configuration {}", path.display()))?
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() as u64 <= MAX_CONFIG_BYTES,
            "gateway configuration exceeds 64 KiB"
        );
        let config: Self =
            serde_json::from_slice(&bytes).context("parse gateway JSON configuration")?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.listen_port >= 1024,
            "gateway listen_port must be between 1024 and 65535"
        );
        ensure!(
            !self.routes.is_empty() && self.routes.len() <= 32,
            "configure between 1 and 32 gateway routes"
        );
        for (host, port) in &self.routes {
            ensure!(
                valid_hostname(host),
                "invalid route {host:?}: use lowercase names such as app.localhost"
            );
            ensure!(
                *port > 0 && *port != self.listen_port,
                "route {host} needs a nonzero upstream port different from listen_port"
            );
        }
        Ok(())
    }
}

fn valid_hostname(host: &str) -> bool {
    host.len() <= 253
        && host.ends_with(".localhost")
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Create the local certificate authority if absent and return its public PEM path.
/// This does not modify operating-system or browser certificate trust.
pub fn init(state_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(state_dir).context("create gateway state directory")?;
    let ca_dir = state_dir.join("gateway-ca");
    if !ca_dir.try_exists()? {
        let staging = tempfile::Builder::new()
            .prefix(".gateway-ca-")
            .tempdir_in(state_dir)?;
        fs::set_permissions(staging.path(), fs::Permissions::from_mode(0o700))?;
        let key = KeyPair::generate().context("generate local CA key")?;
        let now = time::OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "luhmen local development CA");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.name_constraints = Some(NameConstraints {
            permitted_subtrees: vec![GeneralSubtree::DnsName("localhost".to_owned())],
            excluded_subtrees: vec![],
        });
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(3650);
        let cert = params
            .self_signed(&key)
            .context("generate local CA certificate")?;
        write_private(
            &staging.path().join("key.pem"),
            key.serialize_pem().as_bytes(),
        )?;
        write_private(&staging.path().join("ca.pem"), cert.pem().as_bytes())?;
        // The directory rename publishes the key and certificate together. A concurrent
        // initializer may win; its completed identity must be preserved.
        if let Err(error) = fs::rename(staging.path(), &ca_dir)
            && !ca_dir.try_exists()?
        {
            return Err(error).context("publish local CA identity");
        }
        File::open(state_dir)?.sync_all()?;
    }
    load_ca(&ca_dir)
        .context("load local CA identity; existing certificate files were preserved")?;
    Ok(ca_dir.join("ca.pem"))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn load_ca(ca_dir: &Path) -> Result<(Issuer<'static, KeyPair>, CertificateDer<'static>)> {
    let directory = fs::symlink_metadata(ca_dir)?;
    ensure!(
        directory.is_dir() && !directory.file_type().is_symlink(),
        "CA directory must be a real directory"
    );
    ensure!(
        directory.permissions().mode() & 0o077 == 0,
        "CA directory permissions must be 0700"
    );
    for name in ["key.pem", "ca.pem"] {
        let metadata = fs::symlink_metadata(ca_dir.join(name))?;
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "CA {name} must be a regular file"
        );
        ensure!(
            metadata.len() <= MAX_CONFIG_BYTES,
            "CA {name} exceeds 64 KiB"
        );
        if name == "key.pem" {
            ensure!(
                metadata.permissions().mode() & 0o077 == 0,
                "CA private key permissions must be 0600"
            );
        }
    }
    let key = KeyPair::from_pem(&fs::read_to_string(ca_dir.join("key.pem"))?)?;
    let pem = fs::read_to_string(ca_dir.join("ca.pem"))?;
    let cert = CertificateDer::from_pem_slice(pem.as_bytes())
        .context("CA PEM contains no valid certificate")?;
    // Also check that the persisted private key belongs to the persisted certificate.
    rustls::sign::CertifiedKey::from_der(
        vec![cert.clone()],
        PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
        &rustls::crypto::ring::default_provider(),
    )?;
    let issuer = Issuer::from_ca_cert_pem(&pem, key)?;
    Ok((issuer, cert))
}

fn tls_config(state_dir: &Path, routes: &BTreeMap<String, u16>) -> Result<rustls::ServerConfig> {
    init(state_dir)?;
    let (issuer, ca) = load_ca(&state_dir.join("gateway-ca"))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut resolver = ResolvesServerCertUsingSni::new();
    for hostname in routes.keys() {
        let key = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![hostname.clone()])?;
        params
            .distinguished_name
            .push(DnType::CommonName, hostname.clone());
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now - time::Duration::days(1);
        params.not_after = now + time::Duration::days(365);
        let certificate = params.signed_by(&key, &issuer)?;
        let certified_key = rustls::sign::CertifiedKey::from_der(
            vec![certificate.der().clone(), ca.clone()],
            PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            &provider,
        )?;
        resolver.add(hostname, certified_key)?;
    }
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(resolver));
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

/// Run the HTTPS gateway until shutdown is set. Configuration loads once at startup.
/// Shutdown stops accepting connections, allows five seconds to drain, then cancels
/// remaining work. Neither upstream applications nor CA files are removed.
pub fn run(state_dir: &Path, config_path: &Path, shutdown: Cancellation) -> Result<()> {
    let config = Config::read(config_path)?;
    let tls = tls_config(state_dir, &config.routes)?;
    let lock_path = state_dir.join("gateway.lock");
    if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "gateway lock must be a regular file"
        );
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(lock_path)?;
    lock.try_lock()
        .context("another luhmen gateway is using this state directory")?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(serve(config, tls, shutdown))
}

async fn serve(config: Config, tls: rustls::ServerConfig, shutdown: Cancellation) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, config.listen_port))
        .await
        .with_context(|| format!("bind gateway to 127.0.0.1:{}", config.listen_port))?;
    eprintln!(
        "luhmen HTTPS gateway listening on 127.0.0.1:{}",
        config.listen_port
    );
    let config = Arc::new(config);
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let buffers = Arc::new(Semaphore::new(MAX_EXCHANGES));
    let upstream = upstream_client();
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept gateway connection")?;
                let Ok(permit) = permits.clone().try_acquire_owned() else { continue };
                let acceptor = acceptor.clone();
                let config = config.clone();
                let buffers = buffers.clone();
                let upstream = upstream.clone();
                let shutdown = shutdown.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    serve_connection(stream, acceptor, config, buffers, upstream, shutdown).await;
                });
            }
        }
    }
    drop(listener);
    if timeout(Duration::from_secs(5), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }
    Ok(())
}

async fn serve_connection<IO>(
    stream: IO,
    acceptor: TlsAcceptor,
    config: Arc<Config>,
    buffers: Arc<Semaphore>,
    upstream: Upstream,
    shutdown: Cancellation,
) where
    IO: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let Ok(Ok(tls)) = timeout(HANDSHAKE_TIMEOUT, acceptor.accept(stream)).await else {
        return;
    };
    let Some(sni) = tls.get_ref().1.server_name().map(str::to_owned) else {
        return;
    };
    let (deadlines, deadline_updates) = watch::channel(Instant::now() + EXCHANGE_TIMEOUT);
    let service = service_fn(move |request| {
        Box::pin(handle(
            request,
            sni.clone(),
            config.clone(),
            buffers.clone(),
            upstream.clone(),
            deadlines.clone(),
        ))
    });
    let mut http = hyper::server::conn::http1::Builder::new();
    http.max_headers(64)
        .max_buf_size(32 * 1024)
        .timer(TokioTimer::new())
        .header_read_timeout(HANDSHAKE_TIMEOUT);
    let mut connection = http.serve_connection(TokioIo::new(tls), service);
    let result = tokio::select! {
        result = std::future::poll_fn(|cx| connection.poll_without_shutdown(cx)) => result,
        _ = connection_deadline(deadline_updates) => return,
        _ = shutdown.cancelled() => {
            Pin::new(&mut connection).graceful_shutdown();
            std::future::poll_fn(|cx| connection.poll_without_shutdown(cx)).await
        }
    };
    if result.is_ok() {
        let mut parts = connection.into_parts();
        // Hyper yields the transport for upgrade requests even when refused.
        let _ = timeout(HANDSHAKE_TIMEOUT, parts.io.inner_mut().shutdown()).await;
    }
}

async fn connection_deadline(mut deadlines: watch::Receiver<Instant>) {
    loop {
        let deadline = *deadlines.borrow_and_update();
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => return,
            result = deadlines.changed() => if result.is_err() { return; },
        }
    }
}

async fn handle(
    request: Request<Incoming>,
    sni: String,
    config: Arc<Config>,
    buffers: Arc<Semaphore>,
    upstream: Upstream,
    deadlines: watch::Sender<Instant>,
) -> Result<HttpResponse, Infallible> {
    let Ok(permit) = buffers.try_acquire_owned() else {
        return Ok(response(
            StatusCode::SERVICE_UNAVAILABLE,
            "gateway request capacity is busy; retry when requests complete\n",
        ));
    };
    let reservation = Arc::new(permit);
    let deadline = Instant::now() + EXCHANGE_TIMEOUT;
    // The transport may stop polling a response body when a client stops reading.
    // Bound that wait too, without cutting off healthy keep-alive sessions.
    deadlines.send_replace(deadline + HANDSHAKE_TIMEOUT);
    let response = match timeout(
        EXCHANGE_TIMEOUT,
        forward(request, &sni, &config, reservation, &upstream, deadline),
    )
    .await
    {
        Ok(response) => response,
        Err(_) => response(
            StatusCode::GATEWAY_TIMEOUT,
            "gateway exchange exceeded 30 seconds\n",
        ),
    };
    Ok(response)
}

async fn forward(
    request: Request<Incoming>,
    sni: &str,
    config: &Config,
    reservation: Reservation,
    upstream: &Upstream,
    deadline: Instant,
) -> HttpResponse {
    if request.method() == Method::CONNECT || request.headers().contains_key("upgrade") {
        return response(
            StatusCode::NOT_IMPLEMENTED,
            "CONNECT and protocol upgrades are not supported\n",
        );
    }
    if request.uri().scheme().is_some()
        || request.uri().authority().is_some()
        || !request.uri().path().starts_with('/')
    {
        return response(
            StatusCode::BAD_REQUEST,
            "use an origin-form request target\n",
        );
    }
    let Some(host) = request_host(request.headers(), config.listen_port) else {
        return response(
            StatusCode::BAD_REQUEST,
            "provide one valid Host header with the gateway port\n",
        );
    };
    let Some(port) = config.routes.get(&host).copied().filter(|_| host == sni) else {
        return response(
            StatusCode::MISDIRECTED_REQUEST,
            "Host must match the configured TLS server name\n",
        );
    };
    let is_head = request.method() == Method::HEAD;
    let (mut parts, body) = request.into_parts();
    let body = match collect_body(body).await {
        Ok(body) => body,
        Err(_) => {
            return response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body failed or exceeds 8 MiB\n",
            );
        }
    };
    let host_header = parts.headers[HOST].clone();
    strip_hop_headers(&mut parts.headers);
    parts.headers.remove(CONTENT_LENGTH);
    parts.headers.remove("forwarded");
    parts.headers.insert(HOST, host_header.clone());
    parts.headers.insert("x-forwarded-host", host_header);
    parts
        .headers
        .insert("x-forwarded-proto", HeaderValue::from_static("https"));
    parts
        .headers
        .insert("x-forwarded-for", HeaderValue::from_static("127.0.0.1"));
    parts
        .headers
        .insert("x-forwarded-port", HeaderValue::from(config.listen_port));
    let target = parts.uri.path_and_query().map_or("/", |path| path.as_str());
    parts.uri = format!("http://127.0.0.1:{port}{target}")
        .parse()
        .expect("validated loopback port and origin-form target");
    let request = Request::from_parts(
        parts,
        GatewayBody::buffered(body, Some(reservation.clone())),
    );
    match exchange(upstream, request, is_head, reservation, deadline).await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("luhmen gateway upstream 127.0.0.1:{port} failed: {error:#}");
            response(
                StatusCode::BAD_GATEWAY,
                "upstream unavailable or response exceeds 8 MiB\n",
            )
        }
    }
}

fn request_host(headers: &HeaderMap, listen_port: u16) -> Option<String> {
    if headers.get_all(HOST).iter().count() != 1 {
        return None;
    }
    let value = headers.get(HOST)?.to_str().ok()?;
    if value.contains('@') {
        return None;
    }
    let authority: hyper::http::uri::Authority = value.parse().ok()?;
    if authority.port().is_some() && authority.port_u16() != Some(listen_port) {
        return None;
    }
    let host = authority.host().to_ascii_lowercase();
    valid_hostname(&host).then_some(host)
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<HeaderName> = headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|name| HeaderName::from_bytes(name.trim().as_bytes()).ok())
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "proxy-connection",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

async fn exchange(
    upstream: &Upstream,
    request: Request<GatewayBody>,
    is_head: bool,
    reservation: Reservation,
    deadline: Instant,
) -> Result<HttpResponse> {
    let response = upstream
        .request(request)
        .await
        .context("read upstream response")?;
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        bail!("upstream protocol upgrades are not supported");
    }
    let (mut parts, body) = response.into_parts();
    if !is_head && parts.status != StatusCode::NOT_MODIFIED {
        ensure!(
            body.size_hint()
                .upper()
                .is_none_or(|size| size <= MAX_BODY_BYTES as u64),
            "upstream body exceeds 8 MiB"
        );
    }
    strip_hop_headers(&mut parts.headers);
    // Hyper preserves known Content-Length and regenerates chunk framing. Once
    // streamed headers are sent, a late body error closes the connection.
    Ok(Response::from_parts(
        parts,
        GatewayBody::streaming(body, reservation, deadline),
    ))
}

async fn collect_body(mut body: Incoming) -> Result<Bytes> {
    let mut bytes = Vec::new();
    while let Some(frame) = body.frame().await {
        let Ok(chunk) = frame.context("read body frame")?.into_data() else {
            continue;
        };
        ensure!(
            chunk.len() <= MAX_BODY_BYTES - bytes.len(),
            "body exceeds 8 MiB"
        );
        let required = bytes.len() + chunk.len();
        if required > bytes.capacity() {
            // Keep one buffer instead of retaining metadata for every tiny chunk.
            // Explicit growth also prevents a late frame from doubling capacity
            // past the body limit. Bytes takes ownership without another copy.
            let capacity = (bytes.capacity() * 2)
                .max(4096)
                .max(required)
                .min(MAX_BODY_BYTES);
            bytes
                .try_reserve_exact(capacity - bytes.len())
                .context("allocate body buffer")?;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(bytes))
}

fn response(status: StatusCode, message: &'static str) -> HttpResponse {
    let mut response = Response::new(GatewayBody::buffered(
        Bytes::from_static(message.as_bytes()),
        None,
    ));
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CONNECTION, HeaderValue::from_static("close"));
    response
}

#[cfg(test)]
#[path = "../tests/gateway/mod.rs"]
mod tests;
