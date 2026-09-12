#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt, chown};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const CONFIG: &str = "/etc/docker/daemon.json";
const BACKUP: &str = "/etc/docker/.luhmen-shutdown-daemon.json";
const LIMIT: usize = 1024 * 1024;
const O_NOFOLLOW: i32 = 0x0002_0000;
const O_NONBLOCK: i32 = 0x0000_0800;

fn fail<T>(message: impl Into<String>) -> Result<T, String> {
    Err(message.into())
}

fn read_file(path: &Path) -> Result<(Vec<u8>, Metadata), String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("inspect {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > LIMIT as u64 {
        return fail(format!(
            "{} must be a regular file of at most 1 MiB",
            path.display()
        ));
    }
    let mut data = Vec::with_capacity(metadata.len() as usize);
    file.take(LIMIT as u64 + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    if data.len() > LIMIT {
        return fail(format!("{} exceeds 1 MiB", path.display()));
    }
    Ok((data, metadata))
}

#[derive(Clone, Copy)]
struct LocatedValue {
    start: usize,
    end: usize,
    boolean: Option<bool>,
}

struct Json<'a> {
    data: &'a [u8],
    at: usize,
}

impl Json<'_> {
    fn whitespace(&mut self) {
        while self.data.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn byte(&mut self, expected: u8) -> Result<(), String> {
        self.whitespace();
        if self.data.get(self.at) != Some(&expected) {
            return fail("invalid JSON");
        }
        self.at += 1;
        Ok(())
    }

    fn string(&mut self) -> Result<String, String> {
        self.whitespace();
        if self.data.get(self.at) != Some(&b'"') {
            return fail("invalid JSON string");
        }
        let start = self.at;
        self.at += 1;
        while let Some(byte) = self.data.get(self.at).copied() {
            match byte {
                b'"' => {
                    self.at += 1;
                    return decode_json_string(&self.data[start..self.at]);
                }
                b'\\' => {
                    self.at += 2;
                    if self.data.get(self.at - 1) == Some(&b'u') {
                        self.at += 4;
                    }
                }
                0..=0x1f => return fail("control byte in JSON string"),
                _ => self.at += 1,
            }
            if self.at > self.data.len() {
                break;
            }
        }
        fail("unterminated JSON string")
    }

    fn value(&mut self, depth: u8) -> Result<Option<bool>, String> {
        if depth > 64 {
            return fail("JSON nesting exceeds 64 levels");
        }
        self.whitespace();
        match self.data.get(self.at).copied() {
            Some(b'"') => self.string().map(|_| None),
            Some(b'{') => {
                self.at += 1;
                let mut keys = BTreeSet::new();
                self.whitespace();
                if self.data.get(self.at) == Some(&b'}') {
                    self.at += 1;
                    return Ok(None);
                }
                loop {
                    let key = self.string()?;
                    if !keys.insert(key.clone()) {
                        return fail(format!("duplicate Docker configuration key: {key}"));
                    }
                    self.byte(b':')?;
                    self.value(depth + 1)?;
                    self.whitespace();
                    match self.data.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b'}') => {
                            self.at += 1;
                            break;
                        }
                        _ => return fail("invalid JSON object"),
                    }
                }
                Ok(None)
            }
            Some(b'[') => {
                self.at += 1;
                self.whitespace();
                if self.data.get(self.at) == Some(&b']') {
                    self.at += 1;
                    return Ok(None);
                }
                loop {
                    self.value(depth + 1)?;
                    self.whitespace();
                    match self.data.get(self.at) {
                        Some(b',') => self.at += 1,
                        Some(b']') => {
                            self.at += 1;
                            break;
                        }
                        _ => return fail("invalid JSON array"),
                    }
                }
                Ok(None)
            }
            Some(b't') if self.data.get(self.at..self.at + 4) == Some(b"true") => {
                self.at += 4;
                Ok(Some(true))
            }
            Some(b'f') if self.data.get(self.at..self.at + 5) == Some(b"false") => {
                self.at += 5;
                Ok(Some(false))
            }
            Some(b'n') if self.data.get(self.at..self.at + 4) == Some(b"null") => {
                self.at += 4;
                Ok(None)
            }
            Some(b'-' | b'0'..=b'9') => self.number().map(|()| None),
            _ => fail("invalid JSON value"),
        }
    }

    fn number(&mut self) -> Result<(), String> {
        if self.data.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        match self.data.get(self.at) {
            Some(b'0') => {
                self.at += 1;
                if self.data.get(self.at).is_some_and(u8::is_ascii_digit) {
                    return fail("invalid JSON number");
                }
            }
            Some(b'1'..=b'9') => {
                self.at += 1;
                while self.data.get(self.at).is_some_and(u8::is_ascii_digit) {
                    self.at += 1;
                }
            }
            _ => return fail("invalid JSON number"),
        }
        if self.data.get(self.at) == Some(&b'.') {
            self.at += 1;
            let start = self.at;
            while self.data.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            if self.at == start {
                return fail("invalid JSON number");
            }
        }
        if self
            .data
            .get(self.at)
            .is_some_and(|byte| matches!(byte, b'e' | b'E'))
        {
            self.at += 1;
            if self
                .data
                .get(self.at)
                .is_some_and(|byte| matches!(byte, b'+' | b'-'))
            {
                self.at += 1;
            }
            let start = self.at;
            while self.data.get(self.at).is_some_and(u8::is_ascii_digit) {
                self.at += 1;
            }
            if self.at == start {
                return fail("invalid JSON number");
            }
        }
        Ok(())
    }
}

fn decode_json_string(bytes: &[u8]) -> Result<String, String> {
    let mut output = String::new();
    let mut at = 1;
    while at + 1 < bytes.len() {
        if bytes[at] != b'\\' {
            let ch = std::str::from_utf8(&bytes[at..bytes.len() - 1])
                .map_err(|_| "invalid UTF-8 in JSON string")?
                .chars()
                .next()
                .unwrap();
            output.push(ch);
            at += ch.len_utf8();
            continue;
        }
        at += 1;
        match *bytes.get(at).ok_or("invalid JSON escape")? {
            b'"' => output.push('"'),
            b'\\' => output.push('\\'),
            b'/' => output.push('/'),
            b'b' => output.push('\u{8}'),
            b'f' => output.push('\u{c}'),
            b'n' => output.push('\n'),
            b'r' => output.push('\r'),
            b't' => output.push('\t'),
            b'u' => {
                let digits =
                    std::str::from_utf8(bytes.get(at + 1..at + 5).ok_or("invalid Unicode escape")?)
                        .map_err(|_| "invalid Unicode escape")?;
                let value =
                    u32::from_str_radix(digits, 16).map_err(|_| "invalid Unicode escape")?;
                if (0xd800..=0xdbff).contains(&value) {
                    if bytes.get(at + 5..at + 7) != Some(b"\\u") {
                        return fail("unpaired Unicode surrogate");
                    }
                    let low = std::str::from_utf8(
                        bytes.get(at + 7..at + 11).ok_or("invalid Unicode escape")?,
                    )
                    .map_err(|_| "invalid Unicode escape")?;
                    let low = u32::from_str_radix(low, 16).map_err(|_| "invalid Unicode escape")?;
                    if !(0xdc00..=0xdfff).contains(&low) {
                        return fail("unpaired Unicode surrogate");
                    }
                    let scalar = 0x10000 + ((value - 0xd800) << 10) + (low - 0xdc00);
                    output.push(char::from_u32(scalar).ok_or("invalid Unicode scalar")?);
                    at += 10;
                } else {
                    if (0xdc00..=0xdfff).contains(&value) {
                        return fail("unpaired Unicode surrogate");
                    }
                    output.push(char::from_u32(value).ok_or("invalid Unicode scalar")?);
                    at += 4;
                }
            }
            _ => return fail("invalid JSON escape"),
        }
        at += 1;
    }
    Ok(output)
}

fn locate(data: &[u8], wanted: &str) -> Result<(Option<LocatedValue>, usize, bool), String> {
    let mut json = Json { data, at: 0 };
    json.byte(b'{')
        .map_err(|_| "Docker configuration must be a JSON object".to_owned())?;
    let mut found = None;
    let mut keys = BTreeSet::new();
    let mut any = false;
    json.whitespace();
    if json.data.get(json.at) != Some(&b'}') {
        loop {
            any = true;
            let key = json.string()?;
            if !keys.insert(key.clone()) {
                return fail(format!("duplicate Docker configuration key: {key}"));
            }
            json.byte(b':')?;
            json.whitespace();
            let start = json.at;
            let boolean = json.value(1)?;
            if key == wanted {
                found = Some(LocatedValue {
                    start,
                    end: json.at,
                    boolean,
                });
            }
            json.whitespace();
            match json.data.get(json.at) {
                Some(b',') => json.at += 1,
                Some(b'}') => break,
                _ => return fail("invalid JSON object"),
            }
        }
    }
    json.byte(b'}')?;
    let close = json.at - 1;
    json.whitespace();
    if json.at != data.len() {
        return fail("trailing data after JSON object");
    }
    Ok((found, close, any))
}

fn boolean_field(data: &[u8], key: &str) -> Result<Option<bool>, String> {
    locate(data, key)?
        .0
        .map(|value| {
            value
                .boolean
                .ok_or_else(|| format!("Docker {key} must be a boolean"))
        })
        .transpose()
}

fn disabled_config(original: &[u8]) -> Result<Vec<u8>, String> {
    let (located, close, any) = locate(original, "live-restore")?;
    let mut data = Vec::with_capacity(original.len() + 24);
    if let Some(value) = located {
        if value.boolean.is_none() {
            return fail("Docker live-restore must be a boolean");
        }
        data.extend_from_slice(&original[..value.start]);
        data.extend_from_slice(b"false");
        data.extend_from_slice(&original[value.end..]);
    } else {
        data.extend_from_slice(&original[..close]);
        if any {
            data.push(b',');
        }
        data.extend_from_slice(b"\"live-restore\":false");
        data.extend_from_slice(&original[close..]);
    }
    if data.len() > LIMIT {
        return fail("Temporary Docker configuration exceeds 1 MiB");
    }
    Ok(data)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

fn write_temporary(path: &Path, data: &[u8], metadata: &Metadata) -> Result<PathBuf, String> {
    let parent = path
        .parent()
        .ok_or_else(|| "configuration has no parent".to_owned())?;
    let (temporary, mut file) = (0..100)
        .find_map(|number| {
            let candidate =
                parent.join(format!(".luhmen-shutdown-{}-{number}", std::process::id()));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&candidate)
            {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(format!("create {}: {error}", candidate.display()))),
            }
        })
        .transpose()?
        .ok_or_else(|| "too many abandoned Docker shutdown temporary files".to_owned())?;
    let result = (|| {
        chown(&temporary, Some(metadata.uid()), Some(metadata.gid()))
            .map_err(|error| format!("set ownership on {}: {error}", temporary.display()))?;
        file.set_permissions(fs::Permissions::from_mode(metadata.mode() & 0o7777))
            .map_err(|error| format!("set permissions on {}: {error}", temporary.display()))?;
        file.write_all(data).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map(|()| temporary)
}

fn replace_file(
    path: &Path,
    data: &[u8],
    metadata: &Metadata,
    expected: &[u8],
) -> Result<(), String> {
    let temporary = write_temporary(path, data, metadata)?;
    let result = (|| {
        if read_file(path)?.0 != expected {
            return fail(format!(
                "{} changed during shutdown; preserving the external edit",
                path.display()
            ));
        }
        fs::rename(&temporary, path).map_err(|error| error.to_string())?;
        sync_directory(path.parent().unwrap())
    })();
    if temporary.exists() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn save_backup(path: &Path, original: &[u8], metadata: &Metadata) -> Result<(), String> {
    let temporary = write_temporary(path, original, metadata)?;
    let result = fs::hard_link(&temporary, path)
        .map_err(|error| format!("publish shutdown backup: {error}"))
        .and_then(|()| sync_directory(path.parent().unwrap()));
    let _ = fs::remove_file(temporary);
    result
}

fn run(command: &str, arguments: &[&str], timeout: Duration) -> Result<Output, String> {
    let mut child = Command::new(command)
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run {command}: {error}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let output = child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            if status.success() {
                return Ok(output);
            }
            return fail(format!("{command} {} failed", arguments.join(" ")));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return fail(format!("{command} {} timed out", arguments.join(" ")));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn systemctl(arguments: &[&str], timeout: Duration) -> Result<String, String> {
    let output = run("systemctl", arguments, timeout)?;
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn service_state() -> Result<BTreeMap<String, String>, String> {
    Ok(systemctl(
        &[
            "show",
            "docker.service",
            "-p",
            "ActiveState",
            "-p",
            "MainPID",
            "-p",
            "Result",
        ],
        Duration::from_secs(5),
    )?
    .lines()
    .filter_map(|line| line.split_once('='))
    .map(|(key, value)| (key.to_owned(), value.to_owned()))
    .collect())
}

fn live_restore() -> Result<bool, String> {
    let mut stream = UnixStream::connect("/var/run/docker.sock")
        .map_err(|error| format!("connect to Docker: {error}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(1)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(b"GET /info HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .take(LIMIT as u64 + 4097)
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .ok_or_else(|| "Docker /info returned an invalid HTTP response".to_owned())?;
    let headers = std::str::from_utf8(&response[..split]).map_err(|error| error.to_string())?;
    if !headers
        .lines()
        .next()
        .is_some_and(|line| line.contains(" 200 "))
    {
        return fail("Docker /info did not return a successful response");
    }
    let body = &response[split + 4..];
    if body.len() > LIMIT {
        return fail("Docker /info exceeded 1 MiB");
    }
    boolean_field(body, "LiveRestoreEnabled")?
        .ok_or_else(|| "Docker /info did not report LiveRestoreEnabled".to_owned())
}

fn reload_config(expected: bool) -> Result<(), String> {
    let state = service_state()?;
    let pid: u32 = state
        .get("MainPID")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    if state.get("ActiveState").map(String::as_str) != Some("active") || pid <= 1 {
        return fail("Docker is not active enough to reload its configuration");
    }
    run("kill", &["-HUP", &pid.to_string()], Duration::from_secs(5))?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        if live_restore() == Ok(expected) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return fail(
                "Docker did not confirm the live-restore configuration reload within 8 seconds",
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn docker_shims_remain() -> bool {
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    for process in processes.flatten() {
        if !process
            .file_name()
            .to_string_lossy()
            .bytes()
            .all(|byte| byte.is_ascii_digit())
        {
            continue;
        }
        let Ok(arguments) = fs::read(process.path().join("cmdline")) else {
            continue;
        };
        let fields: Vec<_> = arguments.split(|byte| *byte == 0).collect();
        if fields
            .first()
            .is_some_and(|value| value.windows(15).any(|part| part == b"containerd-shim"))
            && fields
                .windows(2)
                .any(|pair| pair[0] == b"-namespace" && pair[1] == b"moby")
        {
            return true;
        }
    }
    false
}

fn restore_file(
    config: &Path,
    original: &[u8],
    metadata: &Metadata,
    temporary: &[u8],
) -> Result<(), String> {
    let current = read_file(config)?.0;
    if current == temporary {
        replace_file(config, original, metadata, temporary)
    } else if current != original {
        fail(
            "Docker configuration changed during shutdown; preserving the external edit and backup",
        )
    } else {
        Ok(())
    }
}

fn restore_runtime(original: &[u8]) -> Result<(), String> {
    let state = service_state()?;
    match state.get("ActiveState").map(String::as_str) {
        Some("active") => reload_config(boolean_field(original, "live-restore")?.unwrap_or(false)),
        Some("inactive" | "failed") if state.get("MainPID").map(String::as_str) == Some("0") => {
            Ok(())
        }
        _ => fail(
            "Docker is still transitioning; original configuration restored and recovery backup retained; wait and retry",
        ),
    }
}

fn recover_backup(config: &Path, backup: &Path) -> Result<(), String> {
    if !backup.exists() && fs::symlink_metadata(backup).is_err() {
        return Ok(());
    }
    let (original, metadata) = read_file(backup)?;
    let temporary = disabled_config(&original)?;
    restore_file(config, &original, &metadata, &temporary)?;
    restore_runtime(&original)?;
    fs::remove_file(backup).map_err(|error| error.to_string())?;
    sync_directory(backup.parent().unwrap())
}

fn shutdown() -> Result<(), String> {
    let config = Path::new(CONFIG);
    let backup = Path::new(BACKUP);
    recover_backup(config, backup)?;
    let state = service_state()?;
    let active = state.get("ActiveState").map(String::as_str) == Some("active");
    if !active
        && !matches!(
            state.get("ActiveState").map(String::as_str),
            Some("inactive" | "failed")
        )
    {
        return fail(format!(
            "Docker is {}; wait for its current operation and retry",
            state
                .get("ActiveState")
                .map(String::as_str)
                .unwrap_or("unknown")
        ));
    }
    let mut restoration: Option<(Vec<u8>, Metadata, Vec<u8>)> = None;
    let result = (|| {
        if active {
            let (original, metadata) = read_file(config)?;
            let temporary = disabled_config(&original)?;
            if live_restore()? {
                save_backup(backup, &original, &metadata)?;
                replace_file(config, &temporary, &metadata, &original)?;
                restoration = Some((original, metadata, temporary));
                reload_config(false)?;
                let (original, metadata, temporary) = restoration.as_ref().unwrap();
                restore_file(config, original, metadata, temporary)?;
            }
            if service_state()?.get("MainPID") != state.get("MainPID") || live_restore()? {
                return fail("Docker changed during shutdown preparation; retry after it settles");
            }
        }
        systemctl(
            &["stop", "docker.service", "docker.socket"],
            Duration::from_secs(150),
        )?;
        let stopped = service_state()?;
        let socket = systemctl(
            &["show", "docker.socket", "-p", "ActiveState", "--value"],
            Duration::from_secs(5),
        )?;
        if stopped.get("ActiveState").map(String::as_str) != Some("inactive")
            || socket.trim() != "inactive"
        {
            return fail("Docker service and socket did not both stop cleanly");
        }
        if active && stopped.get("Result").map(String::as_str) != Some("success") {
            return fail(format!(
                "Docker service shutdown failed: {}",
                stopped
                    .get("Result")
                    .map(String::as_str)
                    .unwrap_or("unknown")
            ));
        }
        if docker_shims_remain() {
            return fail(
                "Docker container processes remain without an active daemon; recover Docker before graceful VM shutdown",
            );
        }
        Ok(())
    })();
    if let Some((original, metadata, temporary)) = restoration {
        if let Err(error) = restore_file(config, &original, &metadata, &temporary).and_then(|()| {
            if result.is_err() {
                restore_runtime(&original)
            } else {
                Ok(())
            }
        }) {
            return fail(error);
        }
        fs::remove_file(backup).map_err(|error| error.to_string())?;
        sync_directory(backup.parent().unwrap())?;
    }
    result
}

fn main() {
    if let Err(error) = shutdown() {
        eprintln!("Graceful Docker shutdown failed: {error}. VM power-off was not requested.");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn disables_live_restore_without_losing_configuration() {
        let value =
            disabled_config(br#"{"features":{"containerd-snapshotter":true},"live-restore":true}"#)
                .unwrap();
        assert_eq!(boolean_field(&value, "live-restore").unwrap(), Some(false));
        assert!(
            std::str::from_utf8(&value)
                .unwrap()
                .contains("containerd-snapshotter")
        );
    }

    #[test]
    fn rejects_non_object_and_invalid_live_restore() {
        assert!(disabled_config(b"[]").unwrap_err().contains("JSON object"));
        assert!(disabled_config(br#"{"value":01}"#).is_err());
        assert!(
            disabled_config(br#"{"live-restore":"yes"}"#)
                .unwrap_err()
                .contains("boolean")
        );
        assert!(
            disabled_config(br#"{"nested":{"key":1,"key":2}}"#)
                .unwrap_err()
                .contains("duplicate")
        );
    }

    #[test]
    fn accepts_unicode_surrogate_pairs_in_unrelated_values() {
        let value = disabled_config(br#"{"label":"\ud83d\ude80"}"#).unwrap();
        assert_eq!(boolean_field(&value, "live-restore").unwrap(), Some(false));
    }

    #[test]
    fn captures_command_output() {
        let output = run(
            "/bin/sh",
            &["-c", "printf captured"],
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(output.stdout, b"captured");
    }

    #[test]
    fn atomic_replacement_preserves_mode_and_expected_contents() {
        let directory = temporary_directory();
        let config = directory.join("daemon.json");
        fs::write(&config, b"original").unwrap();
        fs::set_permissions(&config, fs::Permissions::from_mode(0o640)).unwrap();
        let metadata = config.metadata().unwrap();
        replace_file(&config, b"replacement", &metadata, b"original").unwrap();
        assert_eq!(fs::read(&config).unwrap(), b"replacement");
        assert_eq!(
            config.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_replacement_preserves_an_external_edit() {
        let directory = temporary_directory();
        let config = directory.join("daemon.json");
        fs::write(&config, b"external edit").unwrap();
        let metadata = config.metadata().unwrap();
        let error = replace_file(&config, b"replacement", &metadata, b"original").unwrap_err();
        assert!(error.contains("external edit"));
        assert_eq!(fs::read(&config).unwrap(), b"external edit");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn backup_publication_never_overwrites_recovery_data() {
        let directory = temporary_directory();
        let config = directory.join("daemon.json");
        let backup = directory.join("backup.json");
        fs::write(&config, b"original").unwrap();
        fs::write(&backup, b"older recovery").unwrap();
        let error = save_backup(&backup, b"new recovery", &config.metadata().unwrap()).unwrap_err();
        assert!(error.contains("publish shutdown backup"));
        assert_eq!(fs::read(&backup).unwrap(), b"older recovery");
        fs::remove_dir_all(directory).unwrap();
    }

    fn temporary_directory() -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "luhmen-shutdown-test-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir(&directory).unwrap();
        directory
    }
}
