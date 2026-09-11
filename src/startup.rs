use crate::config;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Running,
    Complete,
    Failed,
    Skipped,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Stage {
    pub name: String,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: u32,
    pub operation: String,
    pub started_unix_ms: u64,
    pub outcome: Outcome,
    pub elapsed_ms: u64,
    pub stages: Vec<Stage>,
}

pub struct Trace {
    path: PathBuf,
    started: Instant,
    report: Report,
}

impl Trace {
    pub fn new(state: &Path, operation: &str) -> Self {
        let trace = Self {
            path: state.join("last-start.json"),
            started: Instant::now(),
            report: Report {
                schema_version: 1,
                operation: operation.to_owned(),
                started_unix_ms: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .try_into()
                    .unwrap_or(u64::MAX),
                outcome: Outcome::Running,
                elapsed_ms: 0,
                stages: Vec::new(),
            },
        };
        trace.checkpoint();
        trace
    }

    pub fn stage<T>(&mut self, name: &str, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.report.stages.push(Stage {
            name: name.to_owned(),
            outcome: Outcome::Running,
            elapsed_ms: 0,
        });
        self.checkpoint();
        let started = Instant::now();
        let result = operation();
        let stage = self.report.stages.last_mut().expect("stage was inserted");
        stage.elapsed_ms = millis(started);
        stage.outcome = if result.is_ok() {
            Outcome::Complete
        } else {
            Outcome::Failed
        };
        self.report.elapsed_ms = millis(self.started);
        if result.is_err() {
            self.report.outcome = Outcome::Failed;
        }
        eprintln!(
            "luhmen: {name} {} after {} ms",
            if result.is_ok() {
                "completed"
            } else {
                "failed"
            },
            stage.elapsed_ms
        );
        self.checkpoint();
        result
    }

    pub fn skip(&mut self, name: &str) {
        self.report.stages.push(Stage {
            name: name.to_owned(),
            outcome: Outcome::Skipped,
            elapsed_ms: 0,
        });
        self.checkpoint();
    }

    pub fn complete(&mut self) {
        self.report.outcome = Outcome::Complete;
        self.report.elapsed_ms = millis(self.started);
        self.checkpoint();
    }

    fn checkpoint(&self) {
        let saved = serde_json::to_vec_pretty(&self.report)
            .map_err(anyhow::Error::from)
            .and_then(|bytes| config::atomic_write(&self.path, &bytes));
        if let Err(error) = saved {
            // Diagnostics must not interrupt a restart between stopping and
            // starting the VM, or replace the dependency's original error.
            eprintln!("luhmen: could not save startup diagnostics: {error:#}");
        }
    }
}

fn millis(started: Instant) -> u64 {
    started.elapsed().as_millis().try_into().unwrap_or(u64::MAX)
}

pub fn read(state: &Path) -> Result<Option<Report>> {
    let path = state.join("last-start.json");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "startup diagnostics must be a regular file"
    );
    let mut bytes = Vec::new();
    File::open(path)?.take(65537).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 65536, "startup diagnostics exceed 64 KiB");
    let report: Report = serde_json::from_slice(&bytes).context("invalid startup diagnostics")?;
    ensure!(
        report.schema_version == 1,
        "unsupported startup diagnostics schema"
    );
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_stage_leaves_a_readable_checkpoint() {
        let temp = tempfile::tempdir().unwrap();
        let mut trace = Trace::new(temp.path(), "start");
        trace
            .stage("lima_start", || {
                let saved = read(temp.path()).unwrap().unwrap();
                assert!(matches!(saved.outcome, Outcome::Running));
                assert!(matches!(saved.stages[0].outcome, Outcome::Running));
                assert_eq!(saved.stages[0].name, "lima_start");
                Ok(())
            })
            .unwrap();
        // No complete() call, as when a process exits between two stages.
        drop(trace);
        let saved = read(temp.path()).unwrap().unwrap();
        assert!(matches!(saved.outcome, Outcome::Running));
        assert!(matches!(saved.stages[0].outcome, Outcome::Complete));
    }

    #[test]
    fn diagnostic_write_failure_preserves_success_and_original_errors() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("last-start.json")).unwrap();
        let mut trace = Trace::new(temp.path(), "restart");
        assert_eq!(trace.stage("lima_stop", || Ok(7)).unwrap(), 7);
        let error = trace
            .stage::<()>("lima_start", || {
                anyhow::bail!("original dependency failure")
            })
            .unwrap_err();
        assert_eq!(error.to_string(), "original dependency failure");
        assert!(temp.path().join("last-start.json").is_dir());
    }
}
