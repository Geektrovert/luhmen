use anyhow::{Context, Result, ensure};
use std::path::Path;
use std::time::Duration;

pub fn ping(socket: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let bytes = runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(2), async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut stream = tokio::net::UnixStream::connect(socket)
                .await
                .context("Docker Engine socket is unavailable")?;
            stream
                .write_all(b"GET /_ping HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .await?;
            let mut bytes = Vec::new();
            stream.take(8193).read_to_end(&mut bytes).await?;
            ensure!(bytes.len() <= 8192, "oversized Engine ping response");
            Ok::<_, anyhow::Error>(bytes)
        })
        .await
        .context("Docker Engine ping exceeded two seconds")?
    })?;
    let response = String::from_utf8(bytes)?;
    let (head, body) = response
        .split_once("\r\n\r\n")
        .context("invalid Engine ping response")?;
    ensure!(
        head.starts_with("HTTP/1.0 200 ") || head.starts_with("HTTP/1.1 200 "),
        "Engine ping returned a non-200 response"
    );
    ensure!(body.trim() == "OK", "Engine ping did not return OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    #[test]
    fn real_socket_checks_engine_response() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            for status in ["200 OK", "503 Unavailable"] {
                let (mut client, _) = listener.accept().unwrap();
                let mut request = [0; 256];
                let n = client.read(&mut request).unwrap();
                assert!(String::from_utf8_lossy(&request[..n]).starts_with("GET /_ping "));
                write!(client, "HTTP/1.0 {status}\r\nContent-Length: 2\r\n\r\nOK").unwrap();
            }
        });
        ping(&path).unwrap();
        assert!(ping(&path).is_err());
        server.join().unwrap();
    }

    #[test]
    fn a_stalled_engine_cannot_hold_readiness_open() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("engine.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let (done, wait) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (_client, _) = listener.accept().unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let start = std::time::Instant::now();
        let error = ping(&path).unwrap_err();
        assert!(error.to_string().contains("exceeded two seconds"));
        assert!(start.elapsed() < Duration::from_secs(3));
        done.send(()).unwrap();
        server.join().unwrap();
    }
}
