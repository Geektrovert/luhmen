use serde_json::json;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

struct Fixture {
    temp: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::Builder::new()
            .prefix("lh-")
            .tempdir_in("/tmp")
            .unwrap();
        let canonical = fs::canonicalize(temp.path()).unwrap();
        let root = canonical.as_path();
        fs::create_dir(root.join("state")).unwrap();
        fs::set_permissions(root.join("state"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(root.join("state/owner"), "luhmen-state-v1\n").unwrap();
        fs::write(
            root.join("state/config.json"),
            serde_json::to_vec(
                &json!({"schema_version":1,"cpus":2,"memory_gib":2,"disk_gib":20,"mounts":[]}),
            )
            .unwrap(),
        )
        .unwrap();
        fs::create_dir_all(root.join("state/lima/luhmen")).unwrap();
        fs::write(root.join("state/lima/luhmen/status"), "Stopped").unwrap();
        fs::write(root.join("state/lima/luhmen/disk"), b"persistent data").unwrap();
        fs::write(root.join("context.json"), serde_json::to_vec(&json!([{"Metadata":{"Description":"luhmen managed Docker Engine (schema 1)"},"Endpoints":{"docker":{"Host":format!("unix://{}/state/lima/luhmen/sock/docker.sock",root.display())}}}])).unwrap()).unwrap();
        executable(
            &root.join("limactl"),
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$FIXTURE/lima-calls"
case "$1" in
  --version) printf 'limactl version 2.2.0\n' ;;
  list) printf '{"name":"luhmen","status":"%s","dir":"%s/luhmen","config":{"env":{"LUHMEN_TEST_VALUE":"fixture-only-do-not-print"}}}\n' "$(cat "$LIMA_HOME/luhmen/status")" "$LIMA_HOME" ;;
  stop) printf stop >> "$FIXTURE/workload-actions"; printf Stopped > "$LIMA_HOME/luhmen/status" ;;
  start) printf start >> "$FIXTURE/workload-actions"; printf Running > "$LIMA_HOME/luhmen/status" ;;
  shell) printf shell >> "$FIXTURE/workload-actions" ;;
  *) exit 8 ;;
esac
"#,
        );
        executable(
            &root.join("docker"),
            r#"#!/bin/sh
set -eu
case "$1 $2" in
  'context ls') printf 'unrelated\nluhmen\n' ;;
  'context inspect') cat "$FIXTURE/context.json" ;;
  '--context luhmen') printf docker >> "$FIXTURE/workload-actions"; printf '%s\n' "$*"; printf 'DOCKER_HOST=%s\nDOCKER_CONTEXT=%s\nBUILDX_BUILDER=%s\n' "${DOCKER_HOST-unset}" "${DOCKER_CONTEXT-unset}" "${BUILDX_BUILDER-unset}" ;;
  *) exit 9 ;;
esac
"#,
        );
        Self { temp }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_luhmen"))
            .args(args)
            .env("HOME", fs::canonicalize(self.temp.path()).unwrap())
            .env("FIXTURE", self.temp.path())
            .env("LUHMEN_HOME", self.temp.path().join("state"))
            .env("LUHMEN_LIMACTL", self.temp.path().join("limactl"))
            .env("LUHMEN_DOCKER", self.temp.path().join("docker"))
            .env("DOCKER_HOST", "unix:///unrelated.sock")
            .env("DOCKER_CONTEXT", "unrelated")
            .env("BUILDX_BUILDER", "unrelated")
            .output()
            .unwrap()
    }
}

fn executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn inspect_reports_vm_and_engine_health_separately() {
    let fixture = Fixture::new();
    fs::write(
        fixture.temp.path().join("state/lima/luhmen/status"),
        "Running",
    )
    .unwrap();
    let output = fixture.run(&["inspect", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "Running");
    assert_eq!(report["engine_ready"], false);
    assert_eq!(report["context_ready"], true);
    assert_eq!(report["config"]["memory_gib"], 2);
    assert!(report["vm"].get("config").is_none());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fixture-only-do-not-print"));
}

#[test]
fn storage_reports_sparse_files_without_calling_runtime_dependencies() {
    let fixture = Fixture::new();
    let disk = fixture.temp.path().join("state/lima/luhmen/disk");
    let file = fs::OpenOptions::new().write(true).open(&disk).unwrap();
    file.set_len(1024 * 1024 * 1024).unwrap();
    let before = fs::metadata(&disk).unwrap();
    let output = fixture.run(&["storage", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["schema_version"], 1);
    assert_eq!(
        report["backing_images"][0]["logical_bytes"],
        1024 * 1024 * 1024
    );
    assert_eq!(report["vm_directory"]["complete"], true);
    assert_eq!(report["shared_lima_cache"]["exists"], false);
    assert!(!fixture.temp.path().join("lima-calls").exists());
    assert!(!fixture.temp.path().join("workload-actions").exists());
    assert_eq!(
        before.modified().unwrap(),
        fs::metadata(disk).unwrap().modified().unwrap()
    );
}

#[test]
fn invalid_startup_history_does_not_hide_current_vm_health() {
    let fixture = Fixture::new();
    let root = fixture.temp.path();
    fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();
    for history in [b"{invalid".as_slice(), &[b' '; 65537]] {
        fs::write(root.join("state/last-start.json"), history).unwrap();
        let output = fixture.run(&["inspect", "--json"]);
        assert!(output.status.success());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["state"], "Running");
        assert_eq!(report["engine_ready"], false);
        assert!(report["last_start"].is_null());
        assert!(
            report["errors"]
                .as_array()
                .unwrap()
                .iter()
                .any(|error| error.as_str().unwrap().starts_with("startup diagnostics:"))
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn starting_an_already_running_vm_records_real_socket_confirmation() {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc::{self, TryRecvError};
    use std::time::{Duration, Instant};
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.temp.path()).unwrap();
    fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();
    fs::create_dir(root.join("state/lima/luhmen/sock")).unwrap();
    let socket = root.join("state/lima/luhmen/sock/docker.sock");
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (shutdown, stopped) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = 0;
        while matches!(stopped.try_recv(), Err(TryRecvError::Empty)) && Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(1)))
                        .unwrap();
                    let mut request = [0; 1024];
                    assert!(stream.read(&mut request).unwrap() > 0);
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                        )
                        .unwrap();
                    requests += 1;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => panic!("{error}"),
            }
        }
        requests
    });
    let output = fixture.run(&["start", "--json"]);
    drop(shutdown);
    assert!(
        server.join().unwrap() > 0,
        "Engine readiness was not probed"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["state"], "Running");
    assert_eq!(report["engine_ready"], true);
    assert_eq!(report["last_start"]["outcome"], "complete");
    let stages = report["last_start"]["stages"].as_array().unwrap();
    assert!(
        stages
            .iter()
            .any(|stage| stage["name"] == "lima_start" && stage["outcome"] == "skipped")
    );
    assert!(
        stages
            .iter()
            .any(|stage| stage["name"] == "engine_socket_ready" && stage["outcome"] == "complete")
    );
    assert!(!root.join("workload-actions").exists());
    let later = fixture.run(&["inspect", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&later.stdout).unwrap();
    assert_eq!(report["last_start"]["outcome"], "complete");
    assert_eq!(
        report["engine_ready"], false,
        "past success must not imply current readiness"
    );
}

#[test]
fn docker_wrapper_uses_only_the_owned_context() {
    let fixture = Fixture::new();
    let output = fixture.run(&["docker", "run", "--rm", "example"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "--context luhmen run --rm example\nDOCKER_HOST=unset\nDOCKER_CONTEXT=unset\nBUILDX_BUILDER=unset\n"
    );
    assert!(
        !fixture
            .run(&["docker", "--context", "unrelated", "ps"])
            .status
            .success()
    );
    assert!(
        !fixture
            .run(&["docker", "context", "use", "unrelated"])
            .status
            .success()
    );
    assert!(
        !fixture
            .run(&["docker", "ps", "--context", "unrelated"])
            .status
            .success()
    );
    assert!(
        !fixture
            .run(&["docker", "ps", "--config=/another/config"])
            .status
            .success()
    );
}

#[test]
fn foreign_context_is_preserved_and_rejected() {
    let fixture = Fixture::new();
    let path = fixture.temp.path().join("context.json");
    let before = b"[{\"Endpoints\":{\"docker\":{\"Host\":\"unix:///unrelated.sock\"}}}]";
    fs::write(&path, before).unwrap();
    let output = fixture.run(&["docker", "ps"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not owned"));
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn symlinked_lima_directories_and_socket_are_rejected_before_workload_actions() {
    for relative in [
        "lima",
        "lima/luhmen",
        "lima/luhmen/sock",
        "lima/luhmen/sock/docker.sock",
    ] {
        let fixture = Fixture::new();
        let root = fs::canonicalize(fixture.temp.path()).unwrap();
        let state = root.join("state");
        fs::write(state.join("lima/luhmen/status"), "Running").unwrap();
        fs::create_dir(state.join("lima/luhmen/sock")).unwrap();
        let path = state.join(relative);
        let foreign = root.join("foreign-target");
        if relative.ends_with("docker.sock") {
            fs::write(&foreign, b"unrelated endpoint").unwrap();
        } else {
            fs::rename(&path, &foreign).unwrap();
        }
        symlink(&foreign, &path).unwrap();

        for args in [
            vec!["inspect", "--json"],
            vec!["docker", "run", "--rm", "example"],
            vec!["shell", "true"],
        ] {
            let output = fixture.run(&args);
            assert!(!output.status.success(), "accepted symlink {relative}");
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("symlink"),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        for action in ["start", "stop", "restart"] {
            let output = fixture.run(&[action]);
            assert!(
                !output.status.success(),
                "{action} accepted symlink {relative}"
            );
            assert!(String::from_utf8_lossy(&output.stderr).contains("symlink"));
        }
        assert!(!root.join("workload-actions").exists());
        assert_eq!(
            fs::read_to_string(state.join("lima/luhmen/status")).unwrap(),
            "Running"
        );
        assert_eq!(
            fs::read(state.join("lima/luhmen/disk")).unwrap(),
            b"persistent data"
        );
        if relative.ends_with("docker.sock") {
            assert_eq!(fs::read(&foreign).unwrap(), b"unrelated endpoint");
        }
    }
}

#[test]
fn inspect_keeps_actual_vm_state_when_config_and_context_diagnostics_fail() {
    for context in [
        b"not valid JSON".as_slice(),
        br#"[{"Endpoints":{"docker":{"Host":"unix:///unrelated.sock"}}}]"#.as_slice(),
    ] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        fs::write(root.join("state/config.json"), b"{ malformed config").unwrap();
        fs::write(root.join("context.json"), context).unwrap();
        fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();

        let output = fixture.run(&["inspect", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["state"], "Running");
        assert_eq!(report["vm"]["status"], "Running");
        assert!(report["config"].is_null());
        assert_eq!(report["context_ready"], false);
        let errors = report["errors"].as_array().unwrap();
        assert!(
            errors
                .iter()
                .any(|error| error.as_str().unwrap().starts_with("configuration:"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.as_str().unwrap().starts_with("Docker context:"))
        );
        assert_eq!(fs::read(root.join("context.json")).unwrap(), context);
        assert!(!root.join("workload-actions").exists());
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn stop_reports_success_when_configuration_and_context_diagnostics_fail() {
    for context in [
        b"not valid JSON".as_slice(),
        br#"[{"Endpoints":{"docker":{"Host":"unix:///unrelated.sock"}}}]"#.as_slice(),
    ] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        let malformed = b"{ malformed config";
        fs::write(root.join("state/config.json"), malformed).unwrap();
        fs::write(root.join("context.json"), context).unwrap();
        fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();

        let output = fixture.run(&["stop", "--json"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["state"], "Stopped");
        assert_eq!(report["vm"]["status"], "Stopped");
        assert_eq!(report["context_ready"], false);
        let errors = report["errors"].as_array().unwrap();
        assert!(
            errors
                .iter()
                .any(|error| error.as_str().unwrap().starts_with("configuration:"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.as_str().unwrap().starts_with("Docker context:"))
        );
        assert_eq!(
            fs::read_to_string(root.join("state/lima/luhmen/status")).unwrap(),
            "Stopped"
        );
        assert_eq!(fs::read(root.join("state/config.json")).unwrap(), malformed);
        assert_eq!(fs::read(root.join("context.json")).unwrap(), context);
        assert_eq!(
            fs::read(root.join("state/lima/luhmen/disk")).unwrap(),
            b"persistent data"
        );
    }
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn restart_refuses_a_missing_mount_before_stopping_the_running_vm() {
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.temp.path()).unwrap();
    let config = json!({
        "schema_version": 1, "cpus": 2, "memory_gib": 2, "disk_gib": 20,
        "mounts": [{ "path": root.join("unavailable-projects"), "writable": true }]
    });
    fs::write(
        root.join("state/config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();

    let output = fixture.run(&["restart", "--json"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("mount directory is missing"));
    assert_eq!(
        fs::read_to_string(root.join("state/lima/luhmen/status")).unwrap(),
        "Running"
    );
    assert!(!root.join("workload-actions").exists());
    assert_eq!(
        fs::read(root.join("state/lima/luhmen/disk")).unwrap(),
        b"persistent data"
    );
    let output = fixture.run(&["inspect", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["last_start"]["operation"], "restart");
    assert_eq!(report["last_start"]["outcome"], "failed");
    assert_eq!(report["last_start"]["stages"][0]["name"], "preflight");
    assert_eq!(report["last_start"]["stages"][0]["outcome"], "failed");
    assert_eq!(report["state"], "Running");
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn stop_is_idempotent_and_force_recovers_broken_state_without_deleting_data() {
    let fixture = Fixture::new();
    let output = fixture.run(&["stop", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let status = fixture.temp.path().join("state/lima/luhmen/status");
    fs::write(&status, "Broken").unwrap();
    assert!(!fixture.run(&["stop"]).status.success());
    assert_eq!(fs::read_to_string(&status).unwrap(), "Broken");
    let output = fixture.run(&["stop", "--force", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(status).unwrap(), "Stopped");
    assert_eq!(
        fs::read(fixture.temp.path().join("state/lima/luhmen/disk")).unwrap(),
        b"persistent data"
    );
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn unavailable_mounts_do_not_prevent_inspection_or_stopping_the_vm() {
    let fixture = Fixture::new();
    let missing_mount = fs::canonicalize(fixture.temp.path())
        .unwrap()
        .join("unavailable-projects");
    let config = json!({
        "schema_version": 1, "cpus": 2, "memory_gib": 2, "disk_gib": 20,
        "mounts": [{ "path": missing_mount, "writable": true }]
    });
    fs::write(
        fixture.temp.path().join("state/config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let status = fixture.temp.path().join("state/lima/luhmen/status");
    fs::write(&status, "Running").unwrap();
    let inspection = fixture.run(&["inspect", "--json"]);
    assert!(
        inspection.status.success(),
        "{}",
        String::from_utf8_lossy(&inspection.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&inspection.stdout).unwrap();
    assert_eq!(report["state"], "Running");
    assert_eq!(report["config"], config);

    let start = fixture.run(&["start"]);
    assert!(!start.status.success());
    assert!(String::from_utf8_lossy(&start.stderr).contains("mount directory is missing"));
    for (state, command) in [
        ("Running", vec!["stop", "--json"]),
        ("Broken", vec!["stop", "--force", "--json"]),
    ] {
        fs::write(&status, state).unwrap();
        let output = fixture.run(&command);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["state"], "Stopped");
        assert_eq!(report["config"], config);
    }
    assert_eq!(
        fs::read(fixture.temp.path().join("state/lima/luhmen/disk")).unwrap(),
        b"persistent data"
    );
}
