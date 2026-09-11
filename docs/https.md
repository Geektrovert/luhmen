# Local HTTPS

`luhmen daemon` proxies HTTPS requests for `.localhost` names to HTTP ports published on the Mac. It listens on `127.0.0.1` and runs in the foreground.

Create a configuration file:

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

The proxy supports buffered HTTP/1 exchanges. WebSockets, CONNECT tunnels, streaming responses, and server-sent events are unsupported.

| Limit | Value |
| --- | --- |
| Routes | 32 |
| Simultaneous connections | 32 |
| Active buffered exchanges | 4 |
| Request or response body | 8 MiB each |
| TLS handshake timeout | 5 seconds |
| Exchange timeout | 30 seconds |

An exchange occupies its slot until the connection finishes, including writes to slow clients. When all four slots are occupied, new requests receive HTTP 503. Each slot reserves 32 MiB of body-buffer capacity. The 128 MiB total excludes headers, TLS, sockets, and runtime overhead.

TLS server names and HTTP `Host` headers must match the same configured route. Unknown TLS names fail the handshake. Upstream connections use loopback directly and ignore host HTTP proxy environment variables.

Use a listen port between 1024 and 65535. Port conflicts fail startup. A stopped upstream application produces a gateway error until it is available again. Routes are explicit; the daemon does not discover container labels or expose Docker Engine's API.
