#![forbid(unsafe_code)]

use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use luhmen::{
    config::{self, Config, Mount},
    gateway,
    process::Runner,
    runtime::{Inspection, Runtime},
};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
}
#[derive(Subcommand)]
enum ConfigAction {
    Show,
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

fn execute(cli: Cli, cancelled: Arc<AtomicBool>) -> Result<()> {
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
                .docker_command()
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
    }
}

fn main() {
    let cli = Cli::parse();
    let cancelled = Arc::new(AtomicBool::new(false));
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
            signal.store(true, Ordering::Relaxed);
            Ok(())
        })();
        if let Err(error) = result {
            eprintln!("luhmen signal handler: {error:#}");
        }
    });
    if let Err(error) = execute(cli, cancelled.clone()) {
        eprintln!("luhmen: {error:#}");
        std::process::exit(if cancelled.load(Ordering::Relaxed) {
            130
        } else {
            1
        });
    }
}
