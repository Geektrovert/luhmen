use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn doctor_explains_missing_lima_and_failed_plugins_without_creating_state() {
    let directory = tempfile::Builder::new()
        .prefix("lh-doctor-")
        .tempdir_in("/tmp")
        .unwrap();
    let docker = directory.path().join("docker");
    fs::write(
        &docker,
        r#"#!/bin/sh
case "$1" in
  --version) printf 'Docker version 29.4.0\n' ;;
  compose) printf 'compose plugin unavailable\n' >&2; exit 1 ;;
  buildx) printf 'buildx plugin unavailable\n' >&2; exit 1 ;;
  *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&docker, fs::Permissions::from_mode(0o700)).unwrap();
    let state = directory.path().join("state");
    let output = Command::new(env!("CARGO_BIN_EXE_luhmen"))
        .args(["doctor", "--json"])
        .env("LUHMEN_HOME", &state)
        .env("LUHMEN_LIMACTL", directory.path().join("missing-limactl"))
        .env("LUHMEN_DOCKER", docker)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["docker"], "Docker version 29.4.0");
    assert!(report["docker_error"].is_null());
    assert_eq!(report["lima_ready"], false);
    assert!(
        report["lima_error"]
            .as_str()
            .unwrap()
            .contains("No such file or directory")
    );
    for plugin in ["compose", "buildx"] {
        assert!(report[plugin].is_null());
        assert!(
            report[format!("{plugin}_error")]
                .as_str()
                .unwrap()
                .contains(&format!("{plugin} plugin unavailable"))
        );
    }
    assert!(!state.exists());
}

#[test]
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn doctor_requires_macos_15_for_nested_virtualization_without_creating_state() {
    let directory = tempfile::Builder::new()
        .prefix("lh-doctor-")
        .tempdir_in("/tmp")
        .unwrap();
    for (name, script) in [
        (
            "sw_vers",
            "#!/bin/sh\nprintf '%s\\n' \"$LUHMEN_TEST_MACOS_VERSION\"\n",
        ),
        ("system_profiler", "#!/bin/sh\nprintf 'Chip: Apple M3\\n'\n"),
    ] {
        let executable = directory.path().join(name);
        fs::write(&executable, script).unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let state = directory.path().join("state");
    for (version, nested_ready) in [("14.7", false), ("15.0", true)] {
        let output = Command::new(env!("CARGO_BIN_EXE_luhmen"))
            .args(["doctor", "--json"])
            .env("PATH", directory.path())
            .env("LUHMEN_TEST_MACOS_VERSION", version)
            .env("LUHMEN_HOME", &state)
            .env("LUHMEN_LIMACTL", directory.path().join("missing-limactl"))
            .env("LUHMEN_DOCKER", directory.path().join("missing-docker"))
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["supported_host"], true, "macOS {version}");
        assert_eq!(report["nested_host_ready"], nested_ready, "macOS {version}");
        if nested_ready {
            assert!(report["nested_host_error"].is_null());
        } else {
            assert!(
                report["nested_host_error"]
                    .as_str()
                    .unwrap()
                    .contains("macOS 15")
            );
        }
        assert!(!state.exists());
    }
}
