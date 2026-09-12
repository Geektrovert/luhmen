#![forbid(unsafe_code)]

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use luhmen::{
    cancel::Cancellation,
    config::{self, Config, Mount},
    gateway,
    microvm::{MicrovmClient, validate_guest_path, validate_id},
    process::Runner,
    runtime::{Inspection, Runtime},
};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "luhmen",
    version,
    about = "Docker on Apple Silicon, powered by Lima",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Create a dedicated VM and Docker context without starting the VM.
    Create {
        #[arg(long, default_value_t = 4)]
        cpus: u16,
        /// RAM limit in GiB.
        #[arg(long, default_value_t = 4)]
        memory: u16,
        /// Sparse virtual disk capacity in GiB.
        #[arg(long, default_value_t = 30)]
        disk: u16,
        /// Share a directory at the same guest path. Read-only unless suffixed :rw.
        #[arg(long, value_name = "DIRECTORY[:ro|rw]")]
        mount: Vec<String>,
        /// Enable the M3+ nested Linux/KVM path for Firecracker microVMs.
        #[arg(long)]
        nested_virtualization: bool,
        #[arg(long)]
        json: bool,
        /// Print the Lima configuration without creating files or a VM.
        #[arg(long)]
        dry_run: bool,
    },
    /// Start the VM and wait for the real Docker Engine API.
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Show actual VM and Engine state.
    Inspect {
        #[arg(long)]
        json: bool,
    },
    /// Stop the VM. Persistent data remains on its disk.
    Stop {
        /// Force termination for a broken VM. May lose unwritten guest data.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Gracefully stop, start, and check Docker readiness.
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Check host and dependency versions without changing a VM.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Report VM disk allocation and shared Lima download-cache usage.
    Storage {
        #[arg(long)]
        json: bool,
    },
    /// Show the saved creation settings.
    Config {
        #[command(subcommand)]
        command: ConfigAction,
    },
    /// Run Docker with the luhmen context. Keeps the global default unchanged.
    Docker {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run a command in the dedicated Linux VM.
    Shell {
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the local HTTPS gateway in the foreground.
    Daemon {
        #[arg(long, value_name = "FILE")]
        config: PathBuf,
    },
    /// Create or print the path to the local gateway CA certificate.
    Cert,
    /// Manage Firecracker microVMs inside the nested Linux VM.
    Microvm {
        #[command(subcommand)]
        command: MicrovmAction,
    },
}
#[derive(Subcommand)]
enum ConfigAction {
    Show,
}

#[derive(Subcommand)]
enum MicrovmAction {
    /// Show nested virtualization and Firecracker prerequisites.
    Capabilities {
        #[arg(long)]
        json: bool,
    },
    /// Register a no-network microVM template inside the Linux guest.
    Create {
        id: String,
        /// Absolute path to an uncompressed Linux kernel inside the Lima guest.
        #[arg(long)]
        kernel: String,
        /// Absolute path to an ext4 root filesystem inside the Lima guest.
        #[arg(long)]
        rootfs: String,
        #[arg(long, default_value_t = 1)]
        vcpus: u16,
        #[arg(long, default_value_t = 512)]
        memory_mib: u32,
        #[arg(long)]
        json: bool,
    },
    /// Start a registered microVM.
    Start {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Stop a microVM and retain its overlay disk.
    Stop {
        id: String,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Inspect one microVM, or all registered microVMs when no id is supplied.
    Inspect {
        id: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

fn report(inspection: Inspection, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(&inspection)?);
    } else {
        println!(
            "luhmen: {}\nDocker Engine: {}\nContext: {}\nEndpoint: {}",
            inspection.state,
            if inspection.engine_ready {
                "ready"
            } else {
                "unavailable"
            },
            if inspection.context_ready {
                "luhmen"
            } else {
                "not created"
            },
            inspection.endpoint
        );
        for error in &inspection.errors {
            eprintln!("luhmen: {error}");
        }
    }
    Ok(())
}

fn execute(cli: Cli, cancelled: Cancellation) -> Result<()> {
    let runtime = Runtime {
        state: config::state_path()?,
        runner: Runner {
            cancelled: cancelled.clone(),
        },
    };
    match cli.command {
        Action::Create {
            cpus,
            memory,
            disk,
            mount,
            nested_virtualization,
            json,
            dry_run,
        } => {
            let mounts = mount
                .iter()
                .map(|value| Mount::parse(value))
                .collect::<Result<Vec<_>>>()?;
            let config = Config {
                schema_version: 1,
                cpus,
                memory_gib: memory,
                disk_gib: disk,
                mounts,
                nested_virtualization,
            };
            if dry_run {
                config.validate(&runtime.state)?;
                println!("{}", luhmen::vm_template::render(&config)?);
                Ok(())
            } else {
                report(runtime.create(config)?, json)
            }
        }
        Action::Start { json } => report(runtime.start()?, json),
        Action::Inspect { json } => report(runtime.inspect()?, json),
        Action::Stop { force, json } => report(runtime.stop(force)?, json),
        Action::Restart { json } => report(runtime.restart()?, json),
        Action::Doctor { json } => {
            let report = runtime.doctor()?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                for (key, value) in report.as_object().context("invalid doctor report")? {
                    println!("{key}: {value}");
                }
            }
            ensure!(
                report["supported_host"] == true
                    && report["lima_ready"] == true
                    && report["docker"].is_string()
                    && report["compose"].is_string()
                    && report["buildx"].is_string(),
                "dependencies are not ready; see doctor report"
            );
            Ok(())
        }
        Action::Storage { json } => {
            let storage = serde_json::to_value(runtime.storage()?)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&storage)?);
            } else {
                for (name, value) in storage.as_object().context("invalid storage report")? {
                    println!("{name}: {value}");
                }
            }
            Ok(())
        }
        Action::Config {
            command: ConfigAction::Show,
        } => {
            config::ensure_owned(&runtime.state, false)?;
            println!("{}", serde_json::to_string_pretty(&runtime.config()?)?);
            Ok(())
        }
        Action::Docker { args } => {
            luhmen::docker_args::validate(&args)?;
            config::ensure_owned(&runtime.state, false)?;
            runtime.check_state_paths()?;
            ensure!(
                runtime.context_ready()?,
                "luhmen context is missing; run `luhmen start`"
            );
            let error = runtime
                .docker_workload_command()
                .args(["--context", "luhmen"])
                .args(args)
                .exec();
            Err(error).context("could not execute Docker CLI")
        }
        Action::Shell { args } => {
            config::ensure_owned(&runtime.state, false)?;
            let error = runtime.shell_command()?.args(args).exec();
            Err(error).context("could not execute guest command")
        }
        Action::Daemon { config: file } => {
            config::ensure_owned(&runtime.state, true)?;
            gateway::run(&runtime.state, &file, cancelled)
        }
        Action::Cert => {
            config::ensure_owned(&runtime.state, true)?;
            println!("{}", gateway::init(&runtime.state)?.display());
            Ok(())
        }
        Action::Microvm { command } => {
            config::ensure_owned(&runtime.state, false)?;
            let client = MicrovmClient::new(runtime.microvm_socket());
            let (request, json) = match command {
                MicrovmAction::Capabilities { json } => ("capabilities".to_owned(), json),
                MicrovmAction::Create {
                    id,
                    kernel,
                    rootfs,
                    vcpus,
                    memory_mib,
                    json,
                } => {
                    validate_id(&id)?;
                    validate_guest_path(&kernel)?;
                    validate_guest_path(&rootfs)?;
                    let parent = runtime.config()?;
                    ensure!(
                        (1..=16).contains(&vcpus),
                        "microVM vcpus must be between 1 and 16"
                    );
                    ensure!(
                        (128..=16_384).contains(&memory_mib),
                        "microVM memory must be between 128 and 16384 MiB"
                    );
                    ensure!(
                        vcpus <= parent.cpus,
                        "microVM vcpus cannot exceed the parent VM CPU count ({})",
                        parent.cpus
                    );
                    ensure!(
                        u64::from(memory_mib) + 512 <= u64::from(parent.memory_gib) * 1024,
                        "microVM memory must leave at least 512 MiB for the parent VM"
                    );
                    (
                        format!(
                            "create id={id} kernel={kernel} rootfs={rootfs} vcpus={vcpus} memory_mib={memory_mib}"
                        ),
                        json,
                    )
                }
                MicrovmAction::Start { id, json } => {
                    validate_id(&id)?;
                    (format!("start id={id}"), json)
                }
                MicrovmAction::Stop { id, force, json } => {
                    validate_id(&id)?;
                    (
                        format!("stop id={id} force={}", if force { 1 } else { 0 }),
                        json,
                    )
                }
                MicrovmAction::Inspect { id, json } => {
                    if let Some(id) = &id {
                        validate_id(id)?;
                    }
                    let request = id
                        .map(|id| format!("inspect id={id}"))
                        .unwrap_or_else(|| "inspect".to_owned());
                    (request, json)
                }
            };
            let value = runtime.microvm_request(&client, &request)?;
            print_microvm_value(value, json)
        }
    }
}

fn print_microvm_value(value: serde_json::Value, json: bool) -> Result<()> {
    match (json, value.as_object()) {
        (false, Some(values)) => {
            for (key, value) in values {
                println!("{key}: {value}");
            }
        }
        _ => println!("{}", serde_json::to_string_pretty(&value)?),
    }
    Ok(())
}

fn main() {
    let cli = Cli::parse();
    let cancelled = Cancellation::new();
    let signal = cancelled.clone();
    std::thread::spawn(move || {
        let result = (|| -> Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            runtime.block_on(async {
                let mut terminate =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
                tokio::select! {
                    result = tokio::signal::ctrl_c() => result?,
                    _ = terminate.recv() => {},
                }
                Ok::<(), anyhow::Error>(())
            })?;
            signal.cancel();
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("luhmen signal handler: {error:#}");
        }
    });
    if let Err(error) = execute(cli, cancelled.clone()) {
        eprintln!("luhmen: {error:#}");
        std::process::exit(if cancelled.is_cancelled() { 130 } else { 1 });
    }
}
