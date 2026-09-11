use crate::config::{self, Config, LIMA_VERSION, NAME};
use crate::engine;
use crate::process::Runner;
use anyhow::{Context, Result, bail, ensure};
use fs2::FileExt;
use serde::Serialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

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
}

impl Runtime {
    pub fn socket(&self) -> PathBuf {
        self.state.join("lima/luhmen/sock/docker.sock")
    }
    pub fn endpoint(&self) -> String {
        format!("unix://{}", self.socket().display())
    }

    fn lima(&self) -> Command {
        let mut command =
            Command::new(std::env::var_os("LUHMEN_LIMACTL").unwrap_or_else(|| "limactl".into()));
        command
            .env("LIMA_HOME", self.state.join("lima"))
            .env("LIMA_CACHE_HOME", self.state.join("cache"))
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
        ] {
            command.env_remove(variable);
        }
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
            self.state.join("lima"),
            self.state.join("lima/luhmen"),
            self.state.join("lima/luhmen/sock"),
        ] {
            match fs::symlink_metadata(&path) {
                Ok(metadata) => ensure!(
                    metadata.is_dir() && !metadata.file_type().is_symlink(),
                    "refusing an unowned or symlinked Lima directory: {}",
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
        ensure!(major >= 14, "macOS 14 or later is required");
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
        Ok(Some(vm))
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
        self.start_locked()?;
        self.inspect()
    }

    fn start_locked(&self) -> Result<()> {
        self.check_lima()?;
        let config = self.config()?;
        config.validate(&self.state)?;
        self.check_resources(&config)?;
        self.ensure_context()?;
        let vm = self
            .vm()?
            .context("VM is missing; run `luhmen create` with the saved configuration")?;
        match vm.get("status").and_then(Value::as_str).unwrap_or("") {
            "Running" => {}
            "Stopped" => {
                ensure!(
                    fs2::available_space(&self.state)? >= 2 * 1024 * 1024 * 1024,
                    "at least 2 GiB of free host storage is required to start the VM"
                );
                eprintln!("Starting the luhmen VM and waiting for Docker Engine.");
                self.runner
                    .run(
                        self.lima()
                            .args(["start", "--tty=false", "--timeout=10m", NAME]),
                        START_TIMEOUT,
                    )?
                    .success("VM start")?;
            }
            status => bail!(
                "VM state is {status:?}; inspect the Lima logs, then use `luhmen stop --force` to recover before retrying"
            ),
        }
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match engine::ping(&self.socket()) {
                Ok(()) => return Ok(()),
                Err(error) if Instant::now() >= deadline => {
                    return Err(error).context(
                        "Docker Engine did not become ready within 120 seconds; VM is preserved",
                    );
                }
                Err(_) => {}
            }
            ensure!(
                !self.runner.cancelled.load(Ordering::Relaxed),
                "readiness cancelled; the VM may still be running; run `luhmen inspect`"
            );
            std::thread::sleep(Duration::from_millis(250));
        }
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
        self.check_lima()?;
        self.config()?.validate(&self.state)?;
        self.check_resources(&self.config()?)?;
        self.context_ready()?;
        ensure!(
            fs2::available_space(&self.state)? >= 2 * 1024 * 1024 * 1024,
            "at least 2 GiB of free host storage is required to restart the VM"
        );
        self.stop_locked(false)?;
        self.start_locked()?;
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
        })
    }

    pub fn doctor(&self) -> Result<Value> {
        let lima = self.check_lima();
        let host = self.check_host();
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
        Ok(
            json!({"schema_version":1,"supported_host":host.is_ok(),"host_error":host.err().map(|e|e.to_string()),"lima_version":LIMA_VERSION,"lima_ready":lima.is_ok(),"lima_error":lima.err().map(|e|e.to_string()),"docker":docker.as_ref().ok().map(|v|v.trim()),"docker_error":docker.err().map(|e|e.to_string()),"compose":compose.as_ref().ok().map(|v|v.trim()),"buildx":buildx.as_ref().ok().map(|v|v.trim()),"free_bytes":fs2::available_space(ancestor)?,"state_directory":self.state,"endpoint":self.endpoint()}),
        )
    }
}
