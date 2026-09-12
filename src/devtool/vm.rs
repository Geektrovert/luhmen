//! Opt-in VM integration checks and measurements.

use anyhow::{Context, Result, bail, ensure};
use clap::Args;
use luhmen::{cancel::Cancellation, process::Runner};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IMAGE: &str =
    "busybox:1.37.0@sha256:f10e809bcf667d8e9f01d2baf82869049a495cd448cdfe1f4dee94078b960ae9";
const CONTEXT_DESCRIPTION: &str = "luhmen managed Docker Engine (schema 1)";
const SMOKE_LABEL: &str = "io.luhmen.smoke.run";
const PERF_LABEL: &str = "io.luhmen.perf.run";

#[derive(Args)]
pub struct SmokeArgs {
    #[arg(long, help = "Required acknowledgement: this restarts the VM")]
    run: bool,
    #[arg(long)]
    fixture_root: PathBuf,
    #[arg(long, default_value = "luhmen")]
    luhmen: PathBuf,
    #[arg(long)]
    recovery: bool,
    #[arg(long)]
    watchers: bool,
    #[arg(long)]
    slow_shutdown: bool,
}

#[derive(Args)]
pub struct PerfArgs {
    #[arg(long, help = "Required acknowledgement: run measurements")]
    run: bool,
    #[arg(long)]
    fixture_root: PathBuf,
    #[arg(long, default_value = "luhmen")]
    luhmen: PathBuf,
    #[arg(long)]
    pull: bool,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=10))]
    iterations: u8,
    #[arg(long, default_value_t = 256, value_parser = clap::value_parser!(u16).range(16..=2048))]
    files: u16,
    #[arg(long, default_value_t = 16, value_parser = clap::value_parser!(u8).range(1..=64))]
    blob_mib: u8,
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=30))]
    idle_seconds: u8,
}

fn emit(check: &str, status: Option<&str>, details: Value) {
    let mut value = details.as_object().cloned().unwrap_or_default();
    value.insert("check".to_owned(), Value::String(check.to_owned()));
    if let Some(status) = status {
        value.insert("status".to_owned(), Value::String(status.to_owned()));
    }
    println!("{}", Value::Object(value));
}

fn run(command: &mut Command, check: bool) -> Result<Output> {
    let display = format!("{command:?}");
    let captured = Runner {
        cancelled: Cancellation::new(),
    }
    .run(command, Duration::from_secs(20 * 60))
    .with_context(|| format!("run {display}"))?;
    let output = Output {
        status: captured.status,
        stdout: captured.stdout.into_bytes(),
        stderr: captured.stderr.into_bytes(),
    };
    if check && !output.status.success() {
        bail!(
            "command failed with {}: {display}\n{}{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
    }
    Ok(output)
}

fn clean_environment(command: &mut Command) {
    for name in [
        "DOCKER_HOST",
        "DOCKER_CONTEXT",
        "DOCKER_TLS_VERIFY",
        "DOCKER_CERT_PATH",
        "BUILDX_BUILDER",
        "BUILDKIT_HOST",
        "DOCKER_DEFAULT_PLATFORM",
        "COMPOSE_PROJECT_NAME",
        "COMPOSE_FILE",
        "COMPOSE_PROFILES",
        "COMPOSE_ENV_FILES",
    ] {
        command.env_remove(name);
    }
}

fn id(prefix: &str) -> String {
    let seed = format!(
        "{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        prefix
    );
    format!(
        "{prefix}-{:x}",
        Sha256::digest(seed)[..6]
            .iter()
            .fold(0_u64, |n, byte| n << 8 | u64::from(*byte))
    )
}

struct Harness {
    luhmen: PathBuf,
    docker: String,
    endpoint: String,
    fixture: PathBuf,
    run_id: String,
    label: &'static str,
}

impl Harness {
    fn command(&self, program: impl AsRef<Path>) -> Command {
        let mut command = Command::new(program.as_ref());
        clean_environment(&mut command);
        command
    }

    fn docker(&self, arguments: &[&str], check: bool) -> Result<Output> {
        self.verify_context()?;
        let mut command = self.command(&self.docker);
        command.args(["--context", "luhmen"]).args(arguments);
        if self.fixture.exists() {
            command.current_dir(&self.fixture);
        }
        run(&mut command, check)
    }

    fn docker_text(&self, arguments: &[&str]) -> Result<String> {
        Ok(String::from_utf8(self.docker(arguments, true)?.stdout)?
            .trim()
            .to_owned())
    }

    fn luhmen(&self, arguments: &[&str]) -> Result<Output> {
        run(self.command(&self.luhmen).args(arguments), true)
    }

    fn guest(&self, arguments: &[&str]) -> Result<String> {
        let mut full = vec!["shell"];
        full.extend_from_slice(arguments);
        Ok(String::from_utf8(self.luhmen(&full)?.stdout)?
            .trim()
            .to_owned())
    }

    fn verify_context(&self) -> Result<()> {
        let contexts: Value = serde_json::from_slice(
            &run(
                self.command(&self.docker)
                    .args(["context", "inspect", "luhmen"]),
                true,
            )?
            .stdout,
        )?;
        ensure!(
            owned_context(&contexts, &self.endpoint),
            "Docker context ownership or endpoint changed; refusing operation"
        );
        Ok(())
    }

    fn assert_owned_containers(&self) -> Result<()> {
        for container in self
            .docker_text(&["ps", "--all", "--quiet", "--no-trunc"])?
            .split_whitespace()
        {
            let data: Value =
                serde_json::from_slice(&self.docker(&["inspect", container], true)?.stdout)?;
            ensure!(
                data[0]["Config"]["Labels"][self.label] == self.run_id,
                "another workload appeared in luhmen; refusing a disruptive operation"
            );
        }
        Ok(())
    }
}

fn owned_context(contexts: &Value, endpoint: &str) -> bool {
    contexts.as_array().is_some_and(|items| {
        items.len() == 1
            && items[0]["Name"] == "luhmen"
            && items[0]["Endpoints"]["docker"]["Host"] == endpoint
            && items[0]["Metadata"]["Description"] == CONTEXT_DESCRIPTION
    })
}

fn preflight(
    luhmen: &Path,
    fixture_root: &Path,
    label: &'static str,
    prefix: &str,
) -> Result<(Harness, Value)> {
    ensure!(
        fixture_root.is_dir(),
        "--fixture-root must be an existing directory"
    );
    let mut command = Command::new(luhmen);
    clean_environment(&mut command);
    let state: Value =
        serde_json::from_slice(&run(command.args(["inspect", "--json"]), true)?.stdout)?;
    ensure!(
        state["schema_version"] == 1
            && state["name"] == "luhmen"
            && state["context"] == "luhmen"
            && state["state"] == "Running"
            && state["context_ready"] == true
            && state["engine_ready"] == true,
        "start a healthy, owned luhmen VM first"
    );
    let vm = &state["vm"];
    let endpoint = state["endpoint"]
        .as_str()
        .context("inspection has no endpoint")?
        .to_owned();
    let directory = vm["dir"]
        .as_str()
        .context("inspection has no VM directory")?;
    ensure!(
        vm["name"] == "luhmen"
            && vm["vmType"] == "vz"
            && vm["arch"] == "aarch64"
            && endpoint == format!("unix://{directory}/sock/docker.sock"),
        "VM identity and Unix socket endpoint do not agree"
    );
    let root = fixture_root.canonicalize()?;
    ensure!(
        state["config"]["mounts"]
            .as_array()
            .is_some_and(|mounts| mounts.iter().any(|mount| {
                mount["writable"] == true
                    && mount["path"]
                        .as_str()
                        .and_then(|path| Path::new(path).canonicalize().ok())
                        .is_some_and(|path| root.starts_with(path))
            })),
        "--fixture-root must be inside a configured writable mount"
    );
    let run_id = id(prefix);
    let harness = Harness {
        luhmen: luhmen.to_path_buf(),
        docker: env::var("LUHMEN_DOCKER").unwrap_or_else(|_| "docker".to_owned()),
        endpoint,
        fixture: root.join(&run_id),
        run_id,
        label,
    };
    ensure!(
        harness.docker_text(&["ps", "--all", "--quiet"])?.is_empty(),
        "remove existing containers before continuing"
    );
    Ok((harness, state))
}

fn engine_ping(endpoint: &str) -> Result<()> {
    let path = endpoint
        .strip_prefix("unix://")
        .context("Docker endpoint is not a Unix socket")?;
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.write_all(b"GET /_ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    ensure!(
        response.starts_with("HTTP/1.1 200") && response.ends_with("OK"),
        "Engine API ping failed"
    );
    Ok(())
}

fn eventually(mut check: impl FnMut() -> Result<()>, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    let mut last = None;
    while Instant::now() < deadline {
        match check() {
            Ok(()) => return Ok(()),
            Err(error) => last = Some(error),
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("readiness deadline expired")))
}

fn http_get(address: &str, port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect((address, port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    ensure!(
        response.starts_with("HTTP/1.1 200"),
        "HTTP fixture returned a non-200 response"
    );
    Ok(response
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_owned())
}

pub fn smoke(args: SmokeArgs) -> Result<()> {
    ensure!(
        args.run,
        "No actions taken. Pass --run to run the suite and restart the luhmen VM."
    );
    let (harness, state) = preflight(
        &args.luhmen,
        &args.fixture_root,
        SMOKE_LABEL,
        "luhmen-check",
    )?;
    let result = smoke_inner(&harness, &state, &args);
    let cleanup = smoke_cleanup(&harness);
    if let Err(error) = &result {
        emit(
            "suite",
            Some("failed"),
            json!({"error": format!("{error:#}")}),
        );
    }
    if let Err(error) = cleanup {
        emit(
            "cleanup",
            Some("failed"),
            json!({"error": format!("{error:#}")}),
        );
        return Err(error);
    }
    result?;
    emit("suite", Some("passed"), json!({}));
    Ok(())
}

fn smoke_inner(h: &Harness, state: &Value, args: &SmokeArgs) -> Result<()> {
    let versions = json!({
        "luhmen": String::from_utf8(h.luhmen(&["--version"])?.stdout)?.trim(),
        "docker": h.docker_text(&["version", "--format", "{{json .}}"])?,
        "compose": h.docker_text(&["compose", "version", "--short"])?,
        "buildx": h.docker_text(&["buildx", "version"])?
    });
    emit(
        "preflight",
        Some("passed"),
        json!({"run_id": h.run_id, "endpoint": h.endpoint, "settings": state["config"], "versions": versions, "base_image": IMAGE}),
    );
    fs::create_dir(&h.fixture)?;
    fs::set_permissions(
        &h.fixture,
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )?;
    let shared = h.fixture.join("shared");
    fs::create_dir(&shared)?;
    let token = id("token");
    fs::write(shared.join("host.txt"), &token)?;
    fs::write(h.fixture.join("index.html"), &token)?;
    fs::write(
        h.fixture.join("Dockerfile"),
        format!(
            "FROM {IMAGE}\nLABEL {SMOKE_LABEL}={}\nCOPY index.html /www/index.html\nRUN test -s /www/index.html && printf built > /built\nCMD [\"httpd\",\"-f\",\"-p\",\"8080\",\"-h\",\"/www\"]\n",
            h.run_id
        ),
    )?;
    fs::write(
        h.fixture.join(".dockerignore"),
        "*\n!Dockerfile\n!index.html\n",
    )?;
    let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    let mut services = json!({"web": {
        "build": {"context": "."}, "image": format!("{}-app:smoke", h.run_id),
        "labels": {SMOKE_LABEL: h.run_id}, "restart": "unless-stopped",
        "ports": [format!("127.0.0.1:{port}:8080")], "cpus": 0.5, "mem_limit": "128m", "pids_limit": 64,
        "volumes": [{"type":"bind","source":shared,"target":"/shared"},{"type":"volume","source":"persistent","target":"/data"}],
        "healthcheck":{"test":["CMD","wget","-q","-O","/dev/null","http://127.0.0.1:8080"],"interval":"2s","timeout":"2s","retries":20}
    }});
    if args.slow_shutdown {
        services["shutdown-proof"] = json!({
            "image": IMAGE, "labels": {SMOKE_LABEL: h.run_id}, "restart":"unless-stopped", "stop_grace_period":"40s",
            "network_mode":"none", "volumes":[{"type":"volume","source":"persistent","target":"/data"}],
            "command":["sh","-c",format!("trap 'if ! test -f /data/shutdown-proof; then sleep 32; fi; printf {token} > /data/shutdown-proof; sync; exit 0' TERM; while :; do sleep 1; done")]
        });
    }
    fs::write(
        h.fixture.join("compose.json"),
        serde_json::to_vec_pretty(
            &json!({"services":services,"volumes":{"persistent":{"labels":{SMOKE_LABEL:h.run_id}}},"networks":{"default":{"labels":{SMOKE_LABEL:h.run_id}}}}),
        )?,
    )?;
    emit(
        "fixture",
        Some("created"),
        json!({"path":h.fixture,"project":h.run_id,"port":port}),
    );
    engine_ping(&h.endpoint)?;
    let info: Value = serde_json::from_str(&h.docker_text(&["info", "--format", "{{json .}}"])?)?;
    ensure!(
        info["NCPU"] == state["config"]["cpus"],
        "guest CPU count differs from configuration"
    );
    emit(
        "engine_api_and_vm_resources",
        Some("passed"),
        json!({"cpus":info["NCPU"],"memory_bytes":info["MemTotal"],"engine_version":info["ServerVersion"],"driver":info["Driver"]}),
    );
    h.docker(
        &[
            "compose",
            "--project-name",
            &h.run_id,
            "--file",
            h.fixture.join("compose.json").to_str().unwrap(),
            "up",
            "--build",
            "--detach",
            "--wait",
            "--wait-timeout",
            "90",
        ],
        true,
    )?;
    let container = h.docker_text(&[
        "compose",
        "--project-name",
        &h.run_id,
        "--file",
        h.fixture.join("compose.json").to_str().unwrap(),
        "ps",
        "--quiet",
        "web",
    ])?;
    eventually(
        || {
            ensure!(
                http_get("127.0.0.1", port, "/")? == token,
                "wrong fixture response"
            );
            Ok(())
        },
        Duration::from_secs(90),
    )?;
    ensure!(
        h.docker_text(&["exec", &container, "cat", "/shared/host.txt"])? == token,
        "container could not read the host mount"
    );
    h.docker(
        &[
            "exec",
            &container,
            "sh",
            "-c",
            "printf %s \"$1\" > /shared/guest.txt",
            "sh",
            &token,
        ],
        true,
    )?;
    ensure!(
        fs::read_to_string(shared.join("guest.txt"))? == token,
        "host could not read the container write"
    );
    h.docker(
        &[
            "exec",
            &container,
            "sh",
            "-c",
            "printf %s \"$1\" > /data/probe",
            "sh",
            &token,
        ],
        true,
    )?;
    h.docker(
        &["exec", &container, "nslookup", "host.docker.internal"],
        true,
    )?;
    h.docker(&["exec", &container, "nslookup", "web."], true)?;
    emit("bind_mounts_and_compose_dns", Some("passed"), json!({}));
    let buildx = h.docker_text(&["buildx", "inspect", "luhmen"])?;
    ensure!(
        buildx.contains("Driver:")
            && buildx.contains("docker")
            && buildx.contains("Endpoint:")
            && buildx.contains("luhmen"),
        "unexpected Buildx builder"
    );
    for iteration in 0..2 {
        let output = h.docker(
            &[
                "buildx",
                "build",
                "--builder",
                "luhmen",
                "--load",
                "--progress",
                "plain",
                "--tag",
                &format!("{}-build:smoke", h.run_id),
                ".",
            ],
            true,
        )?;
        if iteration == 1 {
            ensure!(
                String::from_utf8_lossy(&output.stdout).contains("CACHED")
                    || String::from_utf8_lossy(&output.stderr).contains("CACHED"),
                "Buildx did not report a cached build step"
            );
        }
    }
    emit("buildx_build_and_cache", Some("passed"), json!({}));
    if args.watchers {
        h.guest(&[
            "sudo",
            "apt-get",
            "update",
            "-o",
            "APT::Update::Error-Mode=any",
        ])?;
        h.guest(&[
            "sudo",
            "apt-get",
            "install",
            "-y",
            "--no-install-recommends",
            "inotify-tools",
        ])?;
        let watched = shared.join("watcher.txt");
        fs::write(&watched, "before")?;
        let mut watcher = h.command(&h.luhmen);
        let child = watcher
            .args([
                "shell",
                "timeout",
                "8",
                "inotifywait",
                "--monitor",
                "--format",
                "%e|%w%f",
                "--event",
                "modify,attrib",
                watched.to_str().unwrap(),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        thread::sleep(Duration::from_secs(1));
        fs::write(&watched, "after")?;
        let output = child.wait_with_output()?;
        let events = String::from_utf8(output.stdout)?;
        let observed = events.lines().any(|event| {
            event.ends_with(watched.to_str().unwrap())
                && (event.starts_with("MODIFY") || event.starts_with("ATTRIB"))
        });
        emit(
            "watcher_host_modify",
            Some(if observed { "passed" } else { "limitation" }),
            json!({"events":events.lines().collect::<Vec<_>>(),"note":"Host writes may arrive as ATTRIB through Lima; use polling when events are missing"}),
        );
    }
    h.assert_owned_containers()?;
    h.luhmen(&["restart", "--json"])?;
    eventually(|| engine_ping(&h.endpoint), Duration::from_secs(120))?;
    eventually(
        || {
            ensure!(
                http_get("127.0.0.1", port, "/")? == token,
                "wrong response after restart"
            );
            Ok(())
        },
        Duration::from_secs(90),
    )?;
    ensure!(
        h.docker_text(&["exec", &container, "cat", "/data/probe"])? == token,
        "volume data changed across restart"
    );
    if args.slow_shutdown {
        ensure!(
            h.docker_text(&["exec", &container, "cat", "/data/shutdown-proof"])? == token,
            "container did not complete graceful shutdown"
        );
        ensure!(
            h.docker_text(&["info", "--format", "{{.LiveRestoreEnabled}}"])? == "true",
            "live restore was not restored"
        );
    }
    emit(
        "restart_volume_mount_and_port_persistence",
        Some("passed"),
        json!({}),
    );
    if args.recovery {
        let before = h.guest(&[
            "sudo",
            "systemctl",
            "show",
            "docker.service",
            "--property=MainPID",
            "--value",
        ])?;
        h.guest(&[
            "sudo",
            "systemctl",
            "kill",
            "--kill-whom=main",
            "--signal=SIGKILL",
            "docker.service",
        ])?;
        eventually(
            || {
                let current = h.guest(&[
                    "sudo",
                    "systemctl",
                    "show",
                    "docker.service",
                    "--property=MainPID",
                    "--value",
                ])?;
                ensure!(
                    current != before && current.parse::<u32>().unwrap_or(0) > 1,
                    "waiting for Docker recovery"
                );
                engine_ping(&h.endpoint)
            },
            Duration::from_secs(90),
        )?;
        emit("docker_daemon_failure_recovery", Some("passed"), json!({}));
    }
    Ok(())
}

fn smoke_cleanup(h: &Harness) -> Result<()> {
    if !h.fixture.exists() {
        return Ok(());
    }
    h.verify_context()?;
    let compose = h.fixture.join("compose.json");
    if compose.exists() {
        h.docker(
            &[
                "compose",
                "--project-name",
                &h.run_id,
                "--file",
                compose.to_str().unwrap(),
                "down",
                "--volumes",
                "--timeout",
                "20",
            ],
            true,
        )?;
    }
    for suffix in ["app:smoke", "build:smoke", "proxy:smoke"] {
        let image = format!("{}-{suffix}", h.run_id);
        if h.docker(&["image", "inspect", &image], false)?
            .status
            .success()
        {
            let data: Value =
                serde_json::from_slice(&h.docker(&["image", "inspect", &image], true)?.stdout)?;
            ensure!(
                data[0]["Config"]["Labels"][SMOKE_LABEL] == h.run_id,
                "fixture image ownership changed; refusing removal"
            );
            h.docker(&["image", "rm", &image], true)?;
        }
    }
    emit(
        "cleanup",
        Some("passed"),
        json!({"retained_fixture":h.fixture}),
    );
    Ok(())
}

pub fn perf(args: PerfArgs) -> Result<()> {
    ensure!(
        args.run,
        "No actions taken. Pass --run to measure the otherwise idle VM."
    );
    let (h, state) = preflight(&args.luhmen, &args.fixture_root, PERF_LABEL, "luhmen-perf")?;
    let mut containers = Vec::new();
    let volume = format!("{}-data", h.run_id);
    let result = perf_inner(&h, &state, &args, &mut containers, &volume);
    let cleanup = perf_cleanup(&h, &containers, &volume);
    if let Err(error) = result.and(cleanup) {
        emit(
            "suite",
            Some("failed"),
            json!({"error":format!("{error:#}")}),
        );
        return Err(error);
    }
    emit("suite", Some("passed"), json!({}));
    Ok(())
}

fn perf_inner(
    h: &Harness,
    state: &Value,
    args: &PerfArgs,
    containers: &mut Vec<String>,
    volume: &str,
) -> Result<()> {
    let image = h.docker(&["image", "inspect", IMAGE], false)?;
    if !image.status.success() {
        ensure!(
            args.pull,
            "pinned BusyBox image is missing; pass --pull to download it"
        );
        h.docker(&["pull", "--platform=linux/arm64", IMAGE], true)?;
    }
    fs::create_dir(&h.fixture)?;
    let executable = fs::canonicalize(&h.luhmen).unwrap_or_else(|_| h.luhmen.clone());
    emit(
        "environment",
        None,
        json!({
            "run_id":h.run_id,"endpoint":h.endpoint,"fixture":h.fixture,"measured_at_unix":SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs_f64(),
            "host":{"system":env::consts::OS,"machine":env::consts::ARCH,"logical_cpus":thread::available_parallelism().map(usize::from).ok()},
            "config":state["config"],"lima":state["vm"]["limaVersion"],"image":IMAGE,
            "luhmen":String::from_utf8(h.luhmen(&["--version"])?.stdout)?.trim(),
            "luhmen_entrypoint_sha256":format!("{:x}",Sha256::digest(fs::read(executable)?)),
            "iterations":args.iterations,"files":args.files,"blob_mib":args.blob_mib
        }),
    );
    let memory_before = h.guest(&["cat", "/proc/meminfo"])?;
    thread::sleep(Duration::from_secs(args.idle_seconds.into()));
    emit(
        "idle_resources",
        None,
        json!({"interval_seconds":args.idle_seconds,"guest_memory_before":memory_before,"guest_memory_after":h.guest(&["cat","/proc/meminfo"])?}),
    );
    for iteration in 0..=args.iterations {
        for wrapper in [iteration % 2 == 0, iteration % 2 != 0] {
            let name = format!(
                "{}-start-{}-{iteration}",
                h.run_id,
                if wrapper { "wrapper" } else { "direct" }
            );
            containers.push(name.clone());
            let started = Instant::now();
            let arguments = [
                "run",
                "--name",
                &name,
                "--label",
                &format!("{PERF_LABEL}={}", h.run_id),
                "--platform=linux/arm64",
                "--rm",
                "--network=none",
                IMAGE,
                "true",
            ];
            if wrapper {
                let mut full = vec!["docker"];
                full.extend_from_slice(&arguments);
                h.luhmen(&full)?;
            } else {
                h.docker(&arguments, true)?;
            }
            if iteration > 0 {
                emit(
                    "sample",
                    None,
                    json!({"seconds":started.elapsed().as_secs_f64(),"workload":"warm_container_run_to_exit","invocation":if wrapper{"wrapper"}else{"direct"},"iteration":iteration}),
                );
            }
        }
    }
    h.docker(
        &[
            "volume",
            "create",
            "--label",
            &format!("{PERF_LABEL}={}", h.run_id),
            volume,
        ],
        true,
    )?;
    let bind = format!("type=bind,source={},target=/work", h.fixture.display());
    for (placement, mount) in [
        ("bind", bind),
        (
            "volume",
            format!("type=volume,source={volume},target=/work"),
        ),
    ] {
        let name = format!("{}-{placement}", h.run_id);
        containers.push(name.clone());
        h.docker(
            &[
                "run",
                "--name",
                &name,
                "--label",
                &format!("{PERF_LABEL}={}", h.run_id),
                "--platform=linux/arm64",
                "--detach",
                "--network=none",
                "--mount",
                &mount,
                IMAGE,
                "sleep",
                "900",
            ],
            true,
        )?;
        h.docker(&["exec",&name,"sh","-ec",r#"mkdir -p /work/src; dd if=/dev/zero of=/work/seed bs=4096 count=1 2>/dev/null; i=0; while [ "$i" -lt "$1" ]; do cp /work/seed /work/src/f$i; i=$((i+1)); done"#,"seed",&args.files.to_string()], true)?;
        for iteration in 0..=args.iterations {
            for (workload, script) in [
                (
                    "metadata",
                    "find /work/src -type f -exec stat -c %s {} + | sha256sum",
                ),
                ("read", "find /work/src -type f -exec cat {} + | sha256sum"),
                (
                    "write_fsync",
                    "dd if=/dev/zero of=/work/blob bs=1048576 count=\"$1\" conv=fsync",
                ),
                (
                    "package_build",
                    "tar -czf /work/package.tar.gz -C /work/src .; sha256sum /work/package.tar.gz",
                ),
            ] {
                let started = Instant::now();
                h.docker(
                    &[
                        "exec",
                        &name,
                        "sh",
                        "-ec",
                        script,
                        "workload",
                        &args.blob_mib.to_string(),
                    ],
                    true,
                )?;
                if iteration > 0 {
                    emit(
                        "sample",
                        None,
                        json!({"seconds":started.elapsed().as_secs_f64(),"workload":workload,"placement":placement,"iteration":iteration}),
                    );
                }
            }
        }
    }
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    let http_name = format!("{}-http", h.run_id);
    containers.push(http_name.clone());
    h.docker(
        &[
            "run",
            "--name",
            &http_name,
            "--label",
            &format!("{PERF_LABEL}={}", h.run_id),
            "--platform=linux/arm64",
            "--detach",
            "--publish",
            &format!("127.0.0.1:{port}:8080"),
            IMAGE,
            "sh",
            "-ec",
            &format!(
                "mkdir /www; printf %s {} > /www/index.html; exec httpd -f -p 8080 -h /www",
                h.run_id
            ),
        ],
        true,
    )?;
    eventually(
        || {
            ensure!(
                http_get("127.0.0.1", port, "/")? == h.run_id,
                "wrong HTTP response"
            );
            Ok(())
        },
        Duration::from_secs(20),
    )?;
    for iteration in 1..=u16::from(args.iterations) * 5 {
        let started = Instant::now();
        http_get("127.0.0.1", port, "/")?;
        emit(
            "sample",
            None,
            json!({"workload":"localhost_http_new_connection","iteration":iteration,"seconds":started.elapsed().as_secs_f64(),"port":port}),
        );
    }
    Ok(())
}

fn perf_cleanup(h: &Harness, containers: &[String], volume: &str) -> Result<()> {
    for name in containers.iter().rev() {
        if h.docker(&["container", "inspect", name], false)?
            .status
            .success()
        {
            let info: Value =
                serde_json::from_slice(&h.docker(&["container", "inspect", name], true)?.stdout)?;
            ensure!(
                info[0]["Config"]["Labels"][PERF_LABEL] == h.run_id,
                "container ownership label changed"
            );
            h.docker(&["container", "rm", "--force", name], true)?;
        }
    }
    if h.docker(&["volume", "inspect", volume], false)?
        .status
        .success()
    {
        let info: Value =
            serde_json::from_slice(&h.docker(&["volume", "inspect", volume], true)?.stdout)?;
        ensure!(
            info[0]["Labels"][PERF_LABEL] == h.run_id,
            "volume ownership label changed"
        );
        h.docker(&["volume", "rm", volume], true)?;
    }
    emit(
        "cleanup",
        Some("passed"),
        json!({"retained_fixture":h.fixture}),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_must_match_name_endpoint_and_owner() {
        let endpoint = "unix:///owned/luhmen/sock/docker.sock";
        let context = json!([{"Name":"luhmen","Endpoints":{"docker":{"Host":endpoint}},"Metadata":{"Description":CONTEXT_DESCRIPTION}}]);
        assert!(owned_context(&context, endpoint));
        assert!(!owned_context(&context, "unix:///unrelated.sock"));
    }
}
