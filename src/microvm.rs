use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

const PROTOCOL_VERSION: u32 = 1;
const MAX_RESPONSE_BYTES: usize = 1 << 20;

/// Host-side client for the guest-owned Firecracker manager.
///
/// Each Unix-socket connection carries one bounded, space-separated command and
/// one JSON response. VM lifecycle and filesystem paths are managed in Linux.
#[derive(Clone, Debug)]
pub struct MicrovmClient {
    socket: PathBuf,
}

impl MicrovmClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub fn request(&self, request: &str) -> Result<Value> {
        validate_request(request)?;
        let mut stream = UnixStream::connect(&self.socket)
            .with_context(|| format!("connect to microVM manager at {}", self.socket.display()))?;
        // The first start may copy a disk image before configuring the VMM.
        let response_timeout = if request.split_whitespace().next() == Some("start") {
            Duration::from_secs(300)
        } else {
            Duration::from_secs(20)
        };
        stream
            .set_read_timeout(Some(response_timeout))
            .context("set microVM manager read timeout")?;
        stream
            .set_write_timeout(Some(Duration::from_secs(20)))
            .context("set microVM manager write timeout")?;
        stream
            .write_all(request.as_bytes())
            .context("send microVM manager request")?;
        stream
            .write_all(b"\n")
            .context("finish microVM manager request")?;
        stream.flush().context("flush microVM manager request")?;

        let mut reader = BufReader::new(stream);
        let mut response = Vec::new();
        let mut byte = [0_u8; 1];
        loop {
            let read = reader
                .read(&mut byte)
                .context("read microVM manager response")?;
            ensure!(
                read != 0,
                "microVM manager closed the connection without a response"
            );
            ensure!(
                response.len() < MAX_RESPONSE_BYTES,
                "microVM manager response is too large"
            );
            if byte[0] == b'\n' {
                break;
            }
            response.push(byte[0]);
        }
        let envelope: Response =
            serde_json::from_slice(&response).context("decode microVM manager response")?;
        ensure!(
            envelope.version == PROTOCOL_VERSION,
            "unsupported microVM manager protocol version"
        );
        if !envelope.ok {
            bail!(
                "{}",
                envelope
                    .error
                    .unwrap_or_else(|| "microVM manager request failed".to_owned())
            );
        }
        Ok(envelope.data.unwrap_or(Value::Null))
    }
}

#[derive(Debug, serde::Deserialize)]
struct Response {
    version: u32,
    ok: bool,
    data: Option<Value>,
    error: Option<String>,
}

pub fn validate_id(id: &str) -> Result<()> {
    ensure!(
        !id.is_empty()
            && id.len() <= 64
            && id != "."
            && id != ".."
            && id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-')),
        "microVM id must contain 1 to 64 letters, digits, underscores, dots, or hyphens"
    );
    Ok(())
}

pub fn validate_guest_path(path: &str) -> Result<()> {
    ensure!(
        path.starts_with('/')
            && !path.contains("..")
            && path.len() <= 1024
            && path.bytes().all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'.' | b'/' | b'=' | b':' | b'-')),
        "microVM guest path must be absolute and contain no whitespace, traversal, or unsupported characters"
    );
    Ok(())
}

fn validate_request(request: &str) -> Result<()> {
    ensure!(
        !request.is_empty(),
        "microVM manager request cannot be empty"
    );
    ensure!(
        request.len() <= 4096,
        "microVM manager request is too large"
    );
    ensure!(
        !request.contains(['\n', '\r', '\0', '\'', '"', '\\']),
        "microVM manager request contains unsupported characters"
    );
    let mut tokens = request.split_whitespace();
    let action = tokens.next().context("microVM manager action is missing")?;
    let allowed: &[&str] = match action {
        "capabilities" => &[],
        "inspect" | "start" => &["id"],
        "create" => &["id", "kernel", "rootfs", "vcpus", "memory_mib"],
        "stop" => &["id", "force"],
        _ => bail!("unsupported microVM manager action"),
    };
    let mut seen = HashSet::new();
    for token in tokens {
        ensure!(token.len() <= 1024, "microVM manager argument is too large");
        let (key, value) = token
            .split_once('=')
            .context("microVM manager arguments must be key=value pairs")?;
        ensure!(
            allowed.contains(&key),
            "unsupported microVM manager field: {key}"
        );
        ensure!(seen.insert(key), "duplicate microVM manager field: {key}");
        ensure!(
            !value.is_empty(),
            "microVM manager field cannot be empty: {key}"
        );
        ensure!(
            token.bytes().all(|byte| byte.is_ascii_alphanumeric()
                || matches!(byte, b'_' | b'.' | b'/' | b'=' | b':' | b'-')),
            "microVM manager request contains unsupported characters"
        );
        match key {
            "id" => validate_id(value)?,
            "kernel" | "rootfs" => validate_guest_path(value)?,
            _ => {}
        }
    }
    if matches!(action, "create" | "start" | "stop") {
        ensure!(seen.contains("id"), "microVM manager id is required");
    }
    if action == "create" {
        ensure!(
            seen.contains("kernel") && seen.contains("rootfs"),
            "microVM manager kernel and rootfs are required"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;
    use std::process::{Command, Stdio};
    use std::thread;

    #[test]
    fn client_round_trips_a_versioned_response() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("microvmd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            reader.read_line(&mut request).unwrap();
            assert_eq!(request, "inspect\n");
            let mut stream = stream;
            stream
                .write_all(b"{\"version\":1,\"ok\":true,\"data\":{\"status\":\"created\"}}\n")
                .unwrap();
        });
        let value = MicrovmClient::new(socket).request("inspect").unwrap();
        assert_eq!(value["status"], "created");
        server.join().unwrap();
    }

    #[test]
    fn client_rejects_invalid_requests() {
        for request in [
            "create id=demo kernel=/tmp/kernel rootfs=/tmp/a b",
            "create id=demo;rm",
            "delete id=demo",
            "stop id=foo id=bar force=0",
            "inspect id=bar stop",
            "inspect id=bar kernel=/tmp/unexpected",
            "start id=",
            "start",
        ] {
            assert!(validate_request(request).is_err(), "accepted {request:?}");
        }
    }

    #[test]
    fn client_accepts_the_guest_helpers_actual_responses() {
        let directory = tempfile::Builder::new()
            .prefix("lhmv-")
            .tempdir_in("/tmp")
            .unwrap();
        let root = directory.path();
        let socket = root.join("manager.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let helper = include_str!("../scripts/microvmd.sh")
            .replace("/var/lib/luhmen", &root.join("state").display().to_string())
            .replace("/run/luhmen", &root.join("run").display().to_string());
        fs::write(root.join("helper.sh"), helper).unwrap();
        fs::create_dir(root.join("run")).unwrap();
        fs::create_dir(root.join("bin")).unwrap();
        // This protocol test serves one request at a time. Lifecycle and lock
        // ownership are covered by the guest manager's separate process tests.
        let flock = root.join("bin/flock");
        fs::write(&flock, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&flock, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("kernel"), "kernel").unwrap();
        fs::write(root.join("rootfs"), "rootfs").unwrap();
        let helper = root.join("helper.sh");
        let path = format!(
            "{}:{}",
            root.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let server = thread::spawn(move || {
            for _ in 0..5 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut request)
                    .unwrap();
                let mut child = Command::new("sh")
                    .arg(&helper)
                    .env("PATH", &path)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(request.as_bytes())
                    .unwrap();
                let output = child.wait_with_output().unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                stream.write_all(&output.stdout).unwrap();
            }
        });
        let client = MicrovmClient::new(socket);
        assert_eq!(client.request("inspect").unwrap(), serde_json::json!([]));
        let create = format!(
            "create id=demo kernel={}/kernel rootfs={}/rootfs",
            root.display(),
            root.display()
        );
        assert_eq!(client.request(&create).unwrap()["status"], "created");
        assert_eq!(
            client.request("inspect id=demo").unwrap()["status"],
            "created"
        );
        assert_eq!(client.request("stop id=demo").unwrap()["status"], "stopped");
        assert!(
            client
                .request("inspect id=missing")
                .unwrap_err()
                .to_string()
                .contains("does not exist")
        );
        server.join().unwrap();
    }
}
