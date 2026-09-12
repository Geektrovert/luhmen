use crate::config::{self, Config, LIMA_VERSION, NAME};
use crate::process::Runner;
use crate::{engine, microvm::MicrovmClient, startup, storage};
use anyhow::{Context, Result, anyhow, bail, ensure};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const CONTEXT_DESCRIPTION: &str = "luhmen managed Docker Engine (schema 1)";
const SHORT: Duration = Duration::from_secs(20);
const START_TIMEOUT: Duration = Duration::from_secs(900);

pub struct Runtime {
    pub state: PathBuf,
    pub runner: Runner,
}

#[derive(Debug, Serialize)]
pub struct Inspection {
    pub schema_version: u32,
    pub name: &'static str,
    pub state: String,
    pub engine_ready: bool,
    pub context_ready: bool,
    pub context: &'static str,
    pub endpoint: String,
    pub config: Option<Config>,
    pub vm: Option<Value>,
    pub errors: Vec<String>,
    pub last_start: Option<startup::Report>,
}

impl Runtime {
    pub fn socket(&self) -> PathBuf {
        self.state.join("lima/luhmen/sock/docker.sock")
    }

    pub fn microvm_socket(&self) -> PathBuf {
        self.state.join("lima/luhmen/sock/microvmd.sock")
    }
    pub fn endpoint(&self) -> String {
        format!("unix://{}", self.socket().display())
    }

    fn lima(&self) -> Command {
        let mut command =
            Command::new(std::env::var_os("LUHMEN_LIMACTL").unwrap_or_else(|| "limactl".into()));
        command
            .env("LIMA_HOME", self.state.join("lima"))
            .env_remove("LIMA_INSTANCE")
            .env("LIMA_SSH_PORT_FORWARDER", "false");
        command
    }

    pub fn docker_command(&self) -> Command {
        let mut command =
            Command::new(std::env::var_os("LUHMEN_DOCKER").unwrap_or_else(|| "docker".into()));
        for variable in [
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
            "BUILDX_BUILDER",
            "BUILDKIT_HOST",
        ] {
            command.env_remove(variable);
        }
        command
    }

    pub fn docker_workload_command(&self) -> Command {
        let mut command = self.docker_command();
        // Buildx stores a global builder selection separately from Docker's
        // current context. Keep that state separate and select this VM's builder.
        command
            .env("BUILDX_CONFIG", self.state.join("buildx"))
            .env("BUILDX_BUILDER", NAME);
        command
    }

    pub fn shell_command(&self) -> Result<Command> {
        self.check_lima()?;
        let vm = self.vm()?.context("luhmen VM does not exist")?;
        ensure!(
            vm.get("status").and_then(Value::as_str) == Some("Running"),
            "luhmen VM is not running"
        );
        let mut command = self.lima();
        command.args(["shell", NAME, "--"]);
        Ok(command)
    }

    pub fn check_state_paths(&self) -> Result<()> {
        for path in [
            self.state.join("buildx"),
            self.state.join("lima"),
            self.state.join("lima/luhmen"),
            self.state.join("lima/luhmen/sock"),
        ] {
            match fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "refusing an unowned or symlinked runtime directory: {}",
                    path.display()
                ),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("inspect Lima directory ownership"),
            }
        }
        if let Ok(metadata) = fs::symlink_metadata(self.socket()) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "Engine socket cannot be a symlink"
            );
        }
        if let Ok(metadata) = fs::symlink_metadata(self.microvm_socket()) {
            ensure!(
                !metadata.file_type().is_symlink(),
                "microVM manager socket cannot be a symlink"
            );
        }
        Ok(())
    }

    pub fn check_lima(&self) -> Result<()> {
        self.check_state_paths()?;
        let output = self
            .runner
            .run(self.lima().arg("--version"), SHORT)?
            .success("Lima version check")?;
        let version = output
            .split_whitespace()
            .last()
            .unwrap_or("")
            .trim_start_matches('v');
        ensure!(
            version == LIMA_VERSION,
            "luhmen requires Lima {LIMA_VERSION}; found {}",
            output.trim()
        );
        Ok(())
    }

    fn check_host(&self) -> Result<()> {
        ensure!(
            cfg!(all(target_os = "macos", target_arch = "aarch64")),
            "VM operations require an Apple Silicon Mac running macOS 14 or later"
        );
        ensure!(self.macos_major()? >= 14, "macOS 14 or later is required");
        Ok(())
    }

    fn macos_major(&self) -> Result<u32> {
        let version = self
            .runner
            .run(Command::new("sw_vers").arg("-productVersion"), SHORT)?
            .success("macOS version check")?;
        let major: u32 = version
            .trim()
            .split('.')
            .next()
            .context("missing macOS version")?
            .parse()?;
        Ok(major)
    }

    fn check_nested_host(&self) -> Result<()> {
        ensure!(
            self.macos_major()? >= 15,
            "nested Firecracker support requires macOS 15 or later"
        );
        let output = self
            .runner
            .run(
                Command::new("system_profiler").args(["SPHardwareDataType"]),
                SHORT,
            )?
            .success("Apple Silicon chip check")?;
        let chip = output
            .lines()
            .find_map(|line| line.trim().strip_prefix("Chip: "))
            .context("could not determine the Apple chip model")?;
        let generation = chip
            .strip_prefix("Apple M")
            .and_then(|value| {
                value
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u32>()
                    .ok()
            })
            .context("could not determine the Apple chip generation")?;
        ensure!(
            generation >= 3,
            "nested Firecracker support requires an Apple M3 or later Mac"
        );
        Ok(())
    }

    fn check_resources(&self, config: &Config) -> Result<()> {
        ensure!(
            usize::from(config.cpus) <= std::thread::available_parallelism()?.get(),
            "VM CPU count exceeds the host CPU count"
        );
        let memory = self
            .runner
            .run(Command::new("sysctl").args(["-n", "hw.memsize"]), SHORT)?
            .success("host memory check")?;
        let memory: u64 = memory.trim().parse()?;
        ensure!(
            (u64::from(config.memory_gib) + 2) * 1024 * 1024 * 1024 <= memory,
            "VM memory must leave at least 2 GiB of physical RAM for macOS"
        );
        Ok(())
    }

    fn lock(&self) -> Result<File> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.state.join("operation.lock"))?;
        file.try_lock_exclusive()
            .context("another luhmen lifecycle command is active; wait for it to finish")?;
        Ok(file)
    }

    pub fn config(&self) -> Result<Config> {
        let config: Config = serde_json::from_slice(
            &fs::read(self.state.join("config.json"))
                .context("configuration is missing; run `luhmen create`")?,
        )?;
        config.validate_schema()?;
        Ok(config)
    }

    fn vm(&self) -> Result<Option<Value>> {
        if !self.state.join("lima/luhmen").exists() {
            return Ok(None);
        }
        let output = self
            .runner
            .run(self.lima().args(["list", NAME, "--format=json"]), SHORT)?
            .success("VM inspection")?;
        let mut entries = serde_json::Deserializer::from_str(&output).into_iter::<Value>();
        let Some(vm) = entries.next().transpose()? else {
            return Ok(None);
        };
        ensure!(
            entries.next().is_none(),
            "Lima returned more than one instance"
        );
        ensure!(
            vm.get("name").and_then(Value::as_str) == Some(NAME),
            "Lima returned an unexpected instance"
        );
        let dir = vm
            .get("dir")
            .and_then(Value::as_str)
            .context("Lima instance has no directory")?;
        ensure!(
            Path::new(dir) == self.state.join("lima/luhmen"),
            "Lima instance directory does not match luhmen state"
        );
        // Keep diagnostics useful without exposing Lima's full provisioning scripts,
        // environment variables, or host account metadata through this API.
        let mut summary = serde_json::Map::new();
        for key in [
            "name",
            "dir",
            "status",
            "vmType",
            "arch",
            "cpus",
            "memory",
            "disk",
            "hostname",
            "limaVersion",
            "sshLocalPort",
            "hostAgentPID",
            "driverPID",
            "errors",
        ] {
            if let Some(value) = vm.get(key) {
                summary.insert(key.to_owned(), value.clone());
            }
        }
        Ok(Some(Value::Object(summary)))
    }

    fn context_exists(&self) -> Result<bool> {
        let output = self
            .runner
            .run(
                self.docker_command()
                    .args(["context", "ls", "--format", "{{.Name}}"]),
                SHORT,
            )?
            .success("Docker context list")?;
        Ok(output.lines().any(|name| name == NAME))
    }

    pub fn context_ready(&self) -> Result<bool> {
        if !self.context_exists()? {
            return Ok(false);
        }
        let output = self
            .runner
            .run(
                self.docker_command().args(["context", "inspect", NAME]),
                SHORT,
            )?
            .success("Docker context inspection")?;
        let contexts: Vec<Value> = serde_json::from_str(&output)?;
        ensure!(
            contexts.len() == 1,
            "Docker returned an unexpected context count"
        );
        ensure!(
            contexts[0]
                .pointer("/Endpoints/docker/Host")
                .and_then(Value::as_str)
                == Some(self.endpoint().as_str())
                && contexts[0]
                    .pointer("/Metadata/Description")
                    .and_then(Value::as_str)
                    == Some(CONTEXT_DESCRIPTION),
            "Docker context 'luhmen' already exists and is not owned by this state directory; use the state directory that owns it or resolve the context name collision manually"
        );
        Ok(true)
    }

    fn ensure_context(&self) -> Result<()> {
        if !self.context_ready()? {
            self.runner
                .run(
                    self.docker_command().args([
                        "context",
                        "create",
                        NAME,
                        "--description",
                        CONTEXT_DESCRIPTION,
                        "--docker",
                        &format!("host={}", self.endpoint()),
                    ]),
                    SHORT,
                )?
                .success("Docker context creation")?;
            ensure!(self.context_ready()?, "Docker context was not created");
        }
        Ok(())
    }

    pub fn create(&self, config: Config) -> Result<Inspection> {
        self.check_host()?;
        self.check_lima()?;
        config.validate(&self.state)?;
        if config.nested_virtualization {
            self.check_nested_host()?;
        }
        self.check_resources(&config)?;
        config::ensure_owned(&self.state, true)?;
        let _lock = self.lock()?;
        // Detect collisions before allocating a disk or creating a VM.
        self.context_ready()?;
        if self.state.join("config.json").exists() {
            ensure!(
                self.config()? == config,
                "configuration differs from the created VM; creation settings are immutable"
            );
        } else {
            ensure!(
                !self.state.join("lima/luhmen").exists(),
                "VM exists without a luhmen configuration; refusing to adopt it"
            );
            config::atomic_write(
                &self.state.join("config.json"),
                &serde_json::to_vec_pretty(&config)?,
            )?;
        }
        if self.vm()?.is_none() {
            ensure!(
                fs2::available_space(&self.state)? >= 15 * 1024 * 1024 * 1024,
                "at least 15 GiB of free host storage is required to create a VM"
            );
            fs::create_dir_all(self.state.join("lima"))?;
            let template = crate::vm_template::render(&config)?;
            let template_path = self.state.join("vm.yaml");
            config::atomic_write(&template_path, template.as_bytes())?;
            eprintln!("Creating the luhmen VM. The first download can take several minutes.");
            self.runner
                .run(
                    self.lima()
                        .args(["create", "--name=luhmen", "--tty=false"])
                        .arg(template_path),
                    START_TIMEOUT,
                )?
                .success("VM creation")?;
        }
        self.ensure_context()?;
        self.inspect()
    }

    pub fn start(&self) -> Result<Inspection> {
        self.check_host()?;
        config::ensure_owned(&self.state, false)?;
        let _lock = self.lock()?;
        self.start_locked(false)?;
        self.inspect()
    }

    fn start_locked(&self, restart: bool) -> Result<()> {
        let mut trace = startup::Trace::new(&self.state, if restart { "restart" } else { "start" });
        let running = trace.stage("preflight", || self.start_preflight(restart))?;
        if restart && running {
            trace.stage("lima_stop", || self.stop_locked(false))?;
        }
        if !running || restart {
            trace.stage("lima_start", || {
                eprintln!("Starting the luhmen VM and waiting for Docker Engine.");
                self.runner
                    .run(
                        self.lima()
                            .args(["start", "--tty=false", "--timeout=10m", NAME]),
                        START_TIMEOUT,
                    )?
                    .success("VM start")?;
                Ok(())
            })?;
        } else {
            trace.skip("lima_start");
        }
        trace.stage("engine_socket_ready", || self.wait_for_engine())?;
        trace.complete();
        Ok(())
    }

    fn start_preflight(&self, restart: bool) -> Result<bool> {
        self.check_lima()?;
        let config = self.config()?;
        config.validate(&self.state)?;
        self.check_resources(&config)?;
        self.ensure_context()?;
        let vm = self
            .vm()?
            .context("VM is missing; run `luhmen create` with the saved configuration")?;
        let running = match vm.get("status").and_then(Value::as_str).unwrap_or("") {
            "Running" => true,
            "Stopped" => false,
            status => bail!(
                "VM state is {status:?}; inspect the Lima logs, then use `luhmen stop --force` to recover before retrying"
            ),
        };
        if !running || restart {
            ensure!(
                fs2::available_space(&self.state)? >= 2 * 1024 * 1024 * 1024,
                "at least 2 GiB of free host storage is required to start or restart the VM"
            );
        }
        Ok(running)
    }

    fn wait_for_engine(&self) -> Result<()> {
        tokio::runtime::Builder::new_current_thread().enable_all().build()?.block_on(async {
            let socket = self.socket();
            let ready = async {
                loop {
                    if engine::ping_async(&socket).await.is_ok() { return; }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            };
            tokio::select! {
                _ = self.runner.cancelled.cancelled() => bail!("readiness cancelled; the VM may still be running; run `luhmen inspect`"),
                result = tokio::time::timeout(Duration::from_secs(120), ready) => {
                    result.context("Docker Engine did not become ready within 120 seconds; VM is preserved")?;
                    Ok(())
                }
            }
        })
    }

    fn stop_locked(&self, force: bool) -> Result<()> {
        self.check_lima()?;
        let Some(vm) = self.vm()? else {
            return Ok(());
        };
        let status = vm.get("status").and_then(Value::as_str).unwrap_or("");
        if status == "Stopped" {
            return Ok(());
        }
        ensure!(
            status == "Running" || force,
            "VM is {status:?}; graceful stop is unavailable; use `luhmen stop --force` after inspecting its logs"
        );
        let mut command = self.lima();
        command.arg("stop");
        if force {
            command.arg("--force");
        }
        command.arg(NAME);
        self.runner
            .run(&mut command, Duration::from_secs(180))?
            .success("VM stop")?;
        let state = self.vm()?.context("VM disappeared while stopping")?;
        ensure!(
            state.get("status").and_then(Value::as_str) == Some("Stopped"),
            "Lima did not report a stopped VM"
        );
        Ok(())
    }

    pub fn stop(&self, force: bool) -> Result<Inspection> {
        self.check_host()?;
        config::ensure_owned(&self.state, false)?;
        let _lock = self.lock()?;
        self.stop_locked(force)?;
        self.inspect()
    }

    pub fn restart(&self) -> Result<Inspection> {
        self.check_host()?;
        config::ensure_owned(&self.state, false)?;
        let _lock = self.lock()?;
        self.start_locked(true)?;
        self.inspect()
    }

    pub fn inspect(&self) -> Result<Inspection> {
        let mut errors = Vec::new();
        let mut config = None;
        let mut vm = None;
        let mut state = "NotCreated".to_owned();
        if self.state.exists() {
            config::ensure_owned(&self.state, false)?;
            self.check_state_paths()?;
            if self.state.join("config.json").exists() {
                match self.config() {
                    Ok(value) => config = Some(value),
                    Err(error) => errors.push(format!("configuration: {error:#}")),
                }
            }
            if self.state.join("lima/luhmen").exists() {
                state = "Unknown".to_owned();
                match self.check_lima().and_then(|()| self.vm()) {
                    Ok(value) => vm = value,
                    Err(error) => errors.push(format!("VM inspection: {error:#}")),
                }
                if let Some(status) = vm
                    .as_ref()
                    .and_then(|v| v.get("status"))
                    .and_then(Value::as_str)
                {
                    state = status.to_owned();
                }
            }
        }
        let engine_ready = match engine::ping(&self.socket()) {
            Ok(()) => true,
            Err(error) => {
                if state == "Running" {
                    errors.push(format!("Docker Engine: {error:#}"));
                }
                false
            }
        };
        let context_ready = match self.context_ready() {
            Ok(ready) => ready,
            Err(error) => {
                errors.push(format!("Docker context: {error:#}"));
                false
            }
        };
        let last_start = if self.state.exists() {
            match startup::read(&self.state) {
                Ok(report) => report,
                Err(error) => {
                    errors.push(format!("startup diagnostics: {error:#}"));
                    None
                }
            }
        } else {
            None
        };
        Ok(Inspection {
            schema_version: 1,
            name: NAME,
            state,
            engine_ready,
            context_ready,
            context: NAME,
            endpoint: self.endpoint(),
            config,
            vm,
            errors,
            last_start,
        })
    }

    pub fn storage(&self) -> Result<storage::Report> {
        config::ensure_owned(&self.state, false)?;
        self.check_state_paths()?;
        Ok(storage::inspect(&self.state))
    }

    pub fn microvm_request(&self, client: &MicrovmClient, request: &str) -> Result<Value> {
        let config = self.config()?;
        ensure!(
            config.nested_virtualization,
            "nested virtualization is disabled; recreate the VM with `luhmen create --nested-virtualization`"
        );
        self.check_lima()?;
        let vm = self
            .vm()?
            .context("luhmen VM does not exist; run `luhmen create --nested-virtualization`")?;
        ensure!(
            vm.get("status").and_then(Value::as_str) == Some("Running"),
            "luhmen VM is not running; run `luhmen start`"
        );
        self.check_state_paths()?;
        client.request(request)
    }

    pub fn doctor(&self) -> Result<Value> {
        let lima = self.check_lima();
        let host = self.check_host();
        let nested_host = if host.is_ok() {
            self.check_nested_host()
        } else {
            Err(anyhow!(
                "nested virtualization requires a supported Apple Silicon host"
            ))
        };
        let docker = self
            .runner
            .run(self.docker_command().args(["--version"]), SHORT)
            .and_then(|o| o.success("Docker CLI check"));
        let compose = self
            .runner
            .run(
                self.docker_command()
                    .args(["compose", "version", "--short"]),
                SHORT,
            )
            .and_then(|o| o.success("Compose check"));
        let buildx = self
            .runner
            .run(self.docker_command().args(["buildx", "version"]), SHORT)
            .and_then(|o| o.success("Buildx check"));
        let ancestor = self
            .state
            .ancestors()
            .find(|p| p.exists())
            .context("state path has no existing ancestor")?;
        Ok(json!({
            "schema_version": 1,
            "supported_host": host.is_ok(),
            "host_error": host.err().map(|error| format!("{error:#}")),
            "nested_host_ready": nested_host.is_ok(),
            "nested_host_error": nested_host.err().map(|error| format!("{error:#}")),
            "lima_version": LIMA_VERSION,
            "lima_ready": lima.is_ok(),
            "lima_error": lima.err().map(|error| format!("{error:#}")),
            "docker": docker.as_ref().ok().map(|value| value.trim()),
            "docker_error": docker.err().map(|error| format!("{error:#}")),
            "compose": compose.as_ref().ok().map(|value| value.trim()),
            "compose_error": compose.err().map(|error| format!("{error:#}")),
            "buildx": buildx.as_ref().ok().map(|value| value.trim()),
            "buildx_error": buildx.err().map(|error| format!("{error:#}")),
            "free_bytes": fs2::available_space(ancestor)?,
            "state_directory": self.state,
            "endpoint": self.endpoint(),
        }))
    }
}
