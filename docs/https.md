# Local HTTPS

`luhmen daemon` proxies HTTPS requests for `.localhost` names to HTTP ports published on the Mac. It listens on `127.0.0.1` and runs in the foreground.

Save this configuration as `gateway.json`:

```json
{
  "listen_port": 8443,
  "routes": {
    "app.localhost": 8080
  }
}
```

Start an application on `127.0.0.1:8080`, then start the proxy in another terminal:

```sh
luhmen daemon --config ./gateway.json
```

Open `https://app.localhost:8443`. luhmen does not edit `/etc/hosts` or install a DNS resolver. If your client does not resolve `.localhost` names to loopback, use an explicit mapping such as curl's `--resolve` option.

## Certificates

luhmen creates and keeps its local development certificate authority under `gateway-ca` in its state directory. The private key has file mode `0600`. The CA lasts ten years; application certificates last 365 days and are reissued when the daemon starts. Print the CA certificate path with:

```sh
luhmen cert
```

Use that path to verify a request without changing macOS trust settings:

```sh
curl --cacert /path/printed/by/luhmen/cert \
  --resolve app.localhost:8443:127.0.0.1 \
  https://app.localhost:8443/
```

Browsers report an untrusted certificate until you trust the CA through the browser or macOS Keychain Access. Protect the CA's private key and remove its trust when retiring the runtime. luhmen does not modify trust stores.

## Behavior and limits

Restart the daemon after changing routes. Ctrl-C lets requests drain for five seconds, then cancels remaining connections. It leaves the VM and containers running.

The proxy buffers and validates HTTP/1 uploads before forwarding them. Responses stream as upstream chunks arrive, and client backpressure reaches the upstream. Client and upstream connections can be reused. WebSockets and CONNECT tunnels are unsupported. Response streams, including server-sent events, must fit within the size and exchange limits below.

| Limit | Value |
| --- | --- |
| Routes | 32 |
| Simultaneous connections | 32 |
| Active exchanges | 8 |
| Request or response body | 8 MiB each |
| TLS handshake timeout | 5 seconds |
| Exchange timeout | 30 seconds |

An exchange occupies its slot until its response finishes, including writes to slow clients. When all eight slots are occupied, new requests receive HTTP 503. Upload buffers reserve up to 128 MiB in total, excluding headers, TLS, sockets, and runtime overhead. The upstream pool retains at most one idle connection per loopback port for 30 seconds.

A response with a known oversized length receives HTTP 502 before forwarding. If a streamed response exceeds the limit, times out, or fails after headers have been sent, the proxy terminates that response. Clients must treat it as incomplete.

TLS server names and HTTP `Host` headers must match the same configured route. Unknown TLS names fail the handshake. Upstream connections use loopback directly and ignore host HTTP proxy environment variables.

Use a listen port between 1024 and 65535. Port conflicts fail startup. A stopped upstream application produces a gateway error until it is available again. Routes are explicit; the daemon does not discover container labels or expose Docker Engine's API.
