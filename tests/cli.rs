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
  shell)
    printf shell >> "$FIXTURE/workload-actions"
    if test -e "$FIXTURE/reject-guest-shutdown"; then printf 'Docker shutdown rejected\n' >&2; exit 16; fi
    if test -e "$FIXTURE/wait-guest-shutdown"; then
      : > "$FIXTURE/guest-shutdown-started"
      sleep 30
    fi
    ;;
  validate)
    case "${2##*/}" in
      [A-Za-z0-9]*) ;;
      *) printf 'invalid Lima instance identifier in template filename\n' >&2; exit 17 ;;
    esac
    if test -e "$FIXTURE/reject-validation"; then printf 'invalid candidate\n' >&2; exit 15; fi
    if test -e "$FIXTURE/start-during-validation"; then printf Running > "$LIMA_HOME/luhmen/status"; fi
    cp "$2" "$FIXTURE/validated-template.json"
    ;;
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
  '--context luhmen') printf docker >> "$FIXTURE/workload-actions"; printf '%s\n' "$*"; printf 'DOCKER_HOST=%s\nDOCKER_CONTEXT=%s\nBUILDX_BUILDER=%s\nBUILDKIT_HOST=%s\nBUILDX_CONFIG=%s\n' "${DOCKER_HOST-unset}" "${DOCKER_CONTEXT-unset}" "${BUILDX_BUILDER-unset}" "${BUILDKIT_HOST-unset}" "${BUILDX_CONFIG-unset}" ;;
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
            .env("BUILDKIT_HOST", "tcp://unrelated.invalid:1234")
            .env("BUILDX_CONFIG", self.temp.path().join("unrelated-builders"))
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
fn microvm_capabilities_uses_the_forwarded_guest_manager_socket() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::thread;

    let fixture = Fixture::new();
    fs::write(
        fixture.temp.path().join("state/config.json"),
        serde_json::to_vec(&json!({
            "schema_version": 1,
            "cpus": 2,
            "memory_gib": 2,
            "disk_gib": 20,
            "mounts": [],
            "nested_virtualization": true
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        fixture.temp.path().join("state/lima/luhmen/status"),
        "Running",
    )
    .unwrap();
    let socket_dir = fixture.temp.path().join("state/lima/luhmen/sock");
    fs::create_dir_all(&socket_dir).unwrap();
    let listener = UnixListener::bind(socket_dir.join("microvmd.sock")).unwrap();
    let server = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request = String::new();
        reader.read_line(&mut request).unwrap();
        assert_eq!(request, "capabilities\n");
        let mut stream = stream;
        stream
            .write_all(b"{\"version\":1,\"ok\":true,\"data\":{\"kvm\":true}}\n")
            .unwrap();
    });
    let output = fixture.run(&["microvm", "capabilities", "--json"]);
    server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["kvm"], true);
}

#[test]
fn microvm_arguments_cannot_inject_manager_fields() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;

    for arguments in [
        vec!["microvm", "start", "foo id=bar"],
        vec!["microvm", "stop", "foo id=bar"],
        vec!["microvm", "inspect", "foo id=bar"],
        vec![
            "microvm",
            "create",
            "foo",
            "--kernel",
            "/tmp/kernel id=bar",
            "--rootfs",
            "/tmp/rootfs",
        ],
    ] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        let mut config: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("state/config.json")).unwrap()).unwrap();
        config["nested_virtualization"] = json!(true);
        fs::write(
            root.join("state/config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();
        let socket_dir = root.join("state/lima/luhmen/sock");
        fs::create_dir_all(&socket_dir).unwrap();
        let socket = socket_dir.join("microvmd.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(stream.try_clone().unwrap())
                .read_line(&mut request)
                .unwrap();
            if !request.is_empty() {
                stream
                    .write_all(b"{\"version\":1,\"ok\":true,\"data\":{\"id\":\"bar\"}}\n")
                    .unwrap();
            }
            request
        });
        let output = fixture.run(&arguments);
        // Wake the server after a local rejection. A valid request would already
        // have reached it and received its response before the CLI returned.
        let _ = UnixStream::connect(&socket);
        let request = server.join().unwrap();
        assert!(
            !output.status.success(),
            "accepted {arguments:?}: {request}"
        );
        assert!(
            request.is_empty(),
            "malformed arguments reached the manager: {request}"
        );
    }
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
    use std::time::Duration;
    let fixture = Fixture::new();
    let root = fs::canonicalize(fixture.temp.path()).unwrap();
    fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap();
    fs::create_dir(root.join("state/lima/luhmen/sock")).unwrap();
    let socket = root.join("state/lima/luhmen/sock/docker.sock");
    let listener = UnixListener::bind(socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let (shutdown, stopped) = mpsc::channel::<()>();
    let server = std::thread::spawn(move || {
        let mut requests = 0;
        // Preflight can take longer on a busy runner. Keep the fake Engine alive
        // until the CLI returns and drops the shutdown sender.
        while matches!(stopped.try_recv(), Err(TryRecvError::Empty)) {
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
        format!(
            "--context luhmen run --rm example\nDOCKER_HOST=unset\nDOCKER_CONTEXT=unset\nBUILDX_BUILDER=luhmen\nBUILDKIT_HOST=unset\nBUILDX_CONFIG={}/state/buildx\n",
            fs::canonicalize(fixture.temp.path()).unwrap().display()
        )
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
fn docker_wrapper_rejects_explicit_builder_overrides_before_running_workloads() {
    let fixture = Fixture::new();
    for args in [
        vec!["docker", "buildx", "--builder=unrelated", "build", "."],
        vec!["docker", "build", "--builder", "unrelated", "."],
        vec!["docker", "compose", "build", "--builder=unrelated"],
    ] {
        let output = fixture.run(&args);
        assert!(!output.status.success(), "accepted {args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("builder overrides"));
    }
    assert!(!fixture.temp.path().join("workload-actions").exists());
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
fn symlinked_runtime_directories_and_socket_are_rejected_before_workload_actions() {
    for relative in [
        "buildx",
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
        fs::create_dir(state.join("buildx")).unwrap();
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
fn failed_docker_shutdown_preserves_vm_and_force_bypasses_the_helper() {
    let fixture = Fixture::new();
    let root = fixture.temp.path();
    let status = root.join("state/lima/luhmen/status");
    let original_config = fs::read(root.join("state/config.json")).unwrap();
    fs::write(&status, "Running").unwrap();
    fs::write(root.join("reject-guest-shutdown"), "").unwrap();

    let output = fixture.run(&["stop"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Docker shutdown rejected"));
    assert_eq!(fs::read_to_string(&status).unwrap(), "Running");
    assert_eq!(
        fs::read_to_string(root.join("workload-actions")).unwrap(),
        "shell"
    );
    assert_eq!(
        fs::read(root.join("state/config.json")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(root.join("state/lima/luhmen/disk")).unwrap(),
        b"persistent data"
    );

    fs::remove_file(root.join("workload-actions")).unwrap();
    let output = fixture.run(&["stop", "--force", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fs::read_to_string(&status).unwrap(), "Stopped");
    assert_eq!(
        fs::read_to_string(root.join("workload-actions")).unwrap(),
        "stop"
    );
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn cancelled_docker_shutdown_never_powers_off_the_vm() {
    use std::time::{Duration, Instant};

    let fixture = Fixture::new();
    let root = fixture.temp.path();
    let status = root.join("state/lima/luhmen/status");
    let original_config = fs::read(root.join("state/config.json")).unwrap();
    fs::write(&status, "Running").unwrap();
    fs::write(root.join("wait-guest-shutdown"), "").unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_luhmen"))
        .arg("stop")
        .env("HOME", fs::canonicalize(root).unwrap())
        .env("FIXTURE", root)
        .env("LUHMEN_HOME", root.join("state"))
        .env("LUHMEN_LIMACTL", root.join("limactl"))
        .env("LUHMEN_DOCKER", root.join("docker"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Allow the host and Lima probes to finish under parallel test load. The
    // cancellation contract starts only after the guest command is running.
    let deadline = Instant::now() + Duration::from_secs(30);
    while !root.join("guest-shutdown-started").exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "stop exited before entering guest shutdown"
        );
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("guest shutdown was not called within 30 seconds");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let output = child.wait_with_output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("cancelled"));
    assert_eq!(fs::read_to_string(&status).unwrap(), "Running");
    assert_eq!(
        fs::read_to_string(root.join("workload-actions")).unwrap(),
        "shell"
    );
    assert_eq!(
        fs::read(root.join("state/config.json")).unwrap(),
        original_config
    );
    assert_eq!(
        fs::read(root.join("state/lima/luhmen/disk")).unwrap(),
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

fn update_templates(fixture: &Fixture) -> (serde_json::Value, serde_json::Value) {
    let config: luhmen::config::Config =
        serde_json::from_slice(&fs::read(fixture.temp.path().join("state/config.json")).unwrap())
            .unwrap();
    let current: serde_json::Value =
        serde_json::from_str(&luhmen::vm_template::render(&config).unwrap()).unwrap();
    let mut previous = current.clone();
    previous["provision"][0]["script"] = json!("#!/bin/sh\n# previous provisioning\ntrue\n");
    for path in ["state/vm.yaml", "state/lima/luhmen/lima.yaml"] {
        fs::write(
            fixture.temp.path().join(path),
            serde_json::to_vec_pretty(&previous).unwrap(),
        )
        .unwrap();
    }
    (previous, current)
}

#[test]
fn update_refreshes_only_provisioning_without_starting_the_vm() {
    let fixture = Fixture::new();
    let root = fixture.temp.path();
    let shared = fs::canonicalize(root).unwrap().join("shared workspace");
    fs::create_dir(&shared).unwrap();
    let config = json!({"schema_version":1,"cpus":2,"memory_gib":3,"disk_gib":24,"mounts":[{"path":shared,"writable":true}]});
    fs::write(
        root.join("state/config.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let (mut previous, mut current) = update_templates(&fixture);
    // A saved image selection belongs to this VM. A CLI provisioning update must
    // not replace it with whichever image the new release uses for fresh VMs.
    previous["images"][0]["location"] = json!("https://example.invalid/saved-image.img");
    current["images"] = previous["images"].clone();
    for path in ["state/vm.yaml", "state/lima/luhmen/lima.yaml"] {
        fs::write(root.join(path), serde_json::to_vec(&previous).unwrap()).unwrap();
    }
    let config_bytes = fs::read(root.join("state/config.json")).unwrap();
    let output = fixture.run(&["update", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["updated"], true);
    assert_eq!(report["state"], "Stopped");
    for path in ["state/vm.yaml", "state/lima/luhmen/lima.yaml"] {
        let updated: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join(path)).unwrap()).unwrap();
        assert_eq!(updated, current);
    }
    let validated: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("validated-template.json")).unwrap()).unwrap();
    assert_eq!(validated, current);
    assert_eq!(
        fs::read(root.join("state/config.json")).unwrap(),
        config_bytes
    );
    assert_eq!(
        fs::read(root.join("state/lima/luhmen/disk")).unwrap(),
        b"persistent data"
    );
    assert!(!root.join("workload-actions").exists());
    let output = fixture.run(&["update", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["updated"], false);
}

#[test]
fn update_refuses_running_or_user_edited_vms() {
    for changed in ["running", "provisioning", "mounts", "yaml"] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        let (mut previous, _) = update_templates(&fixture);
        match changed {
            "running" => fs::write(root.join("state/lima/luhmen/status"), "Running").unwrap(),
            "provisioning" => {
                previous["provision"][0]["script"] = json!("#!/bin/sh\necho user-edited\n");
                fs::write(
                    root.join("state/lima/luhmen/lima.yaml"),
                    serde_json::to_vec(&previous).unwrap(),
                )
                .unwrap();
            }
            "mounts" => {
                previous["mounts"] = json!([{"location":"/user-edited","writable":true}]);
                fs::write(
                    root.join("state/lima/luhmen/lima.yaml"),
                    serde_json::to_vec(&previous).unwrap(),
                )
                .unwrap();
            }
            "yaml" => fs::write(root.join("state/vm.yaml"), "vmType: vz\n").unwrap(),
            _ => unreachable!(),
        }
        let actual = fs::read(root.join("state/lima/luhmen/lima.yaml")).unwrap();
        let baseline = fs::read(root.join("state/vm.yaml")).unwrap();
        let output = fixture.run(&["update", "--json"]);
        assert!(!output.status.success(), "{changed} must refuse update");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if changed == "running" {
                "must be stopped"
            } else if changed == "yaml" {
                "JSON"
            } else {
                "differs from the saved"
            }),
            "{changed}: {error}"
        );
        assert_eq!(
            fs::read(root.join("state/lima/luhmen/lima.yaml")).unwrap(),
            actual
        );
        assert_eq!(fs::read(root.join("state/vm.yaml")).unwrap(), baseline);
        assert!(!root.join("workload-actions").exists());
    }
}

#[test]
fn update_recovers_after_only_the_vm_template_was_written() {
    let fixture = Fixture::new();
    let root = fixture.temp.path();
    let (_, current) = update_templates(&fixture);
    fs::write(
        root.join("state/lima/luhmen/lima.yaml"),
        serde_json::to_vec(&current).unwrap(),
    )
    .unwrap();
    let actual = fs::read(root.join("state/lima/luhmen/lima.yaml")).unwrap();
    let output = fixture.run(&["update", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(root.join("state/lima/luhmen/lima.yaml")).unwrap(),
        actual
    );
    let baseline: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("state/vm.yaml")).unwrap()).unwrap();
    assert_eq!(baseline, current);
    assert!(!root.join("workload-actions").exists());
}

#[test]
fn update_preserves_templates_when_validation_fails_or_vm_starts() {
    for trigger in ["reject-validation", "start-during-validation"] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        update_templates(&fixture);
        let before = fs::read(root.join("state/vm.yaml")).unwrap();
        fs::write(root.join(trigger), b"").unwrap();
        let output = fixture.run(&["update"]);
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains(if trigger == "reject-validation" {
                "validation failed"
            } else {
                "must be stopped"
            }),
            "{error}"
        );
        assert_eq!(fs::read(root.join("state/vm.yaml")).unwrap(), before);
        assert_eq!(
            fs::read(root.join("state/lima/luhmen/lima.yaml")).unwrap(),
            before
        );
        assert!(!root.join("workload-actions").exists());
    }
}

#[test]
fn update_refuses_symlinked_templates() {
    for template in ["state/vm.yaml", "state/lima/luhmen/lima.yaml"] {
        let fixture = Fixture::new();
        let root = fixture.temp.path();
        update_templates(&fixture);
        let path = root.join(template);
        let before = fs::read(&path).unwrap();
        let external = root.join("external-template.json");
        fs::rename(&path, &external).unwrap();
        symlink(&external, &path).unwrap();
        let output = fixture.run(&["update"]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("regular file"));
        assert!(fs::symlink_metadata(path).unwrap().file_type().is_symlink());
        assert_eq!(fs::read(external).unwrap(), before);
        assert!(!root.join("workload-actions").exists());
    }
}
