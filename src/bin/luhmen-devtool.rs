#![forbid(unsafe_code)]

#[path = "../devtool/distribution.rs"]
mod distribution;
#[path = "../devtool/vm.rs"]
mod vm;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "cargo devtool",
    about = "Development and release tools for luhmen"
)]
struct Cli {
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Build the release executable with local source paths remapped.
    BuildRelease {
        #[arg(long)]
        target: Option<String>,
    },
    /// Reject machine-specific home paths in product source.
    CheckSource,
    /// Copy license texts supplied with locked Cargo dependencies.
    CollectLicenses {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        target: Option<String>,
    },
    /// Inspect generated Cargo archives for generated files.
    CheckCargoPackage {
        #[arg(required = true)]
        archives: Vec<PathBuf>,
    },
    /// Build deterministic macOS ARM64 release archives.
    Release {
        #[arg(long)]
        output: PathBuf,
    },
    /// Run destructive integration checks against an otherwise idle luhmen VM.
    VmSmoke(vm::SmokeArgs),
    /// Measure an otherwise idle luhmen VM.
    VmPerf(vm::PerfArgs),
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Action::BuildRelease { target } => {
            let repo = distribution::repo();
            let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| repo.join("target"));
            let (executable, _) =
                distribution::build_release(&repo, &target_dir, target.as_deref())?;
            eprintln!("Built luhmen with local source paths remapped.");
            println!("{}", executable.display());
        }
        Action::CheckSource => distribution::check_source()?,
        Action::CollectLicenses { output, target } => {
            distribution::collect_licenses(&output, target.as_deref())?
        }
        Action::CheckCargoPackage { archives } => {
            for archive in archives {
                let entries = distribution::check_cargo_package(&archive)?;
                println!(
                    "Cargo archive inventory passed: {} ({entries} entries).",
                    archive.display()
                );
            }
        }
        Action::Release { output } => distribution::release(&output)?,
        Action::VmSmoke(args) => vm::smoke(args)?,
        Action::VmPerf(args) => vm::perf(args)?,
    }
    Ok(())
}
