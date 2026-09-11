use anyhow::{Context, Result, bail};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

const MAX_OUTPUT: u64 = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct Runner {
    pub cancelled: Arc<AtomicBool>,
}

pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(self, operation: &str) -> Result<String> {
        if !self.status.success() {
            bail!(
                "{operation} failed ({}): {}",
                self.status,
                self.stderr.trim()
            );
        }
        Ok(self.stdout)
    }
}

impl Runner {
    pub fn run(&self, command: &mut Command, timeout: Duration) -> Result<Output> {
        if self.cancelled.load(Ordering::Relaxed) {
            bail!("operation cancelled");
        }
        let mut stdout = tempfile::tempfile()?;
        let mut stderr = tempfile::tempfile()?;
        command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?);
        let mut child = command
            .spawn()
            .context("could not start dependency command")?;
        let start = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            let cancelled = self.cancelled.load(Ordering::Relaxed);
            let timed_out = start.elapsed() > timeout;
            let excessive_output = stdout.metadata()?.len() + stderr.metadata()?.len() > MAX_OUTPUT;
            if cancelled || timed_out || excessive_output {
                terminate_group(&mut child)?;
                let reason = if cancelled {
                    "cancelled"
                } else if timed_out {
                    "timed out"
                } else {
                    "exceeded output limit"
                };
                bail!(
                    "dependency command {reason}; VM state may have changed; run `luhmen inspect` before retrying"
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        Ok(Output {
            status,
            stdout: read_output(&mut stdout)?,
            stderr: read_output(&mut stderr)?,
        })
    }
}

fn terminate_group(child: &mut Child) -> Result<()> {
    // Lima gives its detached hostagent a separate process group. This group owns
    // only the invocation and its ordinary helpers. Keep the leader unreaped until
    // both signals are sent so its PID cannot be reused during cleanup.
    let group = format!("-{}", child.id());
    let signal = |name: &str| -> std::io::Result<ExitStatus> {
        Command::new("/bin/kill")
            .args([name, &group])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
    };
    if signal("-TERM").is_err() {
        let _ = child.kill();
        let _ = child.wait();
        bail!("could not signal dependency process group");
    }
    std::thread::sleep(Duration::from_millis(250));
    let _ = signal("-KILL");
    child.wait().context("wait for cancelled dependency")?;
    Ok(())
}

fn read_output(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_OUTPUT + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_OUTPUT as usize {
        bail!("dependency output exceeds 4 MiB");
    }
    String::from_utf8(bytes).context("dependency output is not UTF-8")
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runner() -> Runner {
        Runner {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
    #[test]
    fn captures_exit_status_and_both_streams() {
        let output = runner()
            .run(
                Command::new("sh").args(["-c", "printf hello; printf problem >&2; exit 7"]),
                Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "problem");
        assert_eq!(output.status.code(), Some(7));
    }
    #[test]
    fn timeout_and_cancellation_are_bounded() {
        let start = Instant::now();
        assert!(
            runner()
                .run(Command::new("sleep").arg("30"), Duration::from_millis(100))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(3));
        let runner = runner();
        runner.cancelled.store(true, Ordering::Relaxed);
        assert!(
            runner
                .run(Command::new("true").arg(""), Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn timeout_stops_child_helpers_before_releasing_the_operation() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("survived");
        let script = "sh -c 'sleep 1; printf survived > \"$1\"' helper \"$1\" & wait";
        let result = runner().run(
            Command::new("sh")
                .args(["-c", script, "parent"])
                .arg(&marker),
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!marker.exists(), "child helper survived cancellation");
    }
}
