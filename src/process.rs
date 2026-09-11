use crate::cancel::Cancellation;
use anyhow::{Context, Result, bail, ensure};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;
use tokio::io::AsyncReadExt;

const MAX_OUTPUT: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct Runner {
    pub cancelled: Cancellation,
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
        ensure!(!self.cancelled.is_cancelled(), "operation cancelled");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(self.run_async(command, timeout))
    }

    async fn run_async(&self, command: &mut Command, timeout: Duration) -> Result<Output> {
        // Register before spawning so a short-lived child cannot exit before
        // notification is enabled. SIGCHLD is a hint; try_wait checks this child.
        let mut exits = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
        let mut child = command
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("could not start dependency command")?;
        let capture = self.capture(&mut child, &mut exits, timeout).await;
        let (status, stdout, stderr) = match capture {
            Ok(capture) => capture,
            Err(error) => {
                let error = match terminate_group(&mut child, &mut exits).await {
                    Ok(()) => error,
                    Err(cleanup) => {
                        error.context(format!("dependency cleanup also failed: {cleanup:#}"))
                    }
                };
                return Err(error).context(
                    "dependency command failed; VM state may have changed; run `luhmen inspect` before retrying",
                );
            }
        };
        Ok(Output {
            status,
            stdout: String::from_utf8(stdout).context("dependency stdout is not UTF-8")?,
            stderr: String::from_utf8(stderr).context("dependency stderr is not UTF-8")?,
        })
    }

    async fn capture(
        &self,
        child: &mut Child,
        exits: &mut tokio::signal::unix::Signal,
        timeout: Duration,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
        let mut stdout = tokio::process::ChildStdout::from_std(
            child
                .stdout
                .take()
                .context("dependency stdout is unavailable")?,
        )?;
        let mut stderr = tokio::process::ChildStderr::from_std(
            child
                .stderr
                .take()
                .context("dependency stderr is unavailable")?,
        )?;
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        let (mut output, mut errors) = (Vec::new(), Vec::new());
        let (mut output_closed, mut errors_closed) = (false, false);
        let (mut output_chunk, mut error_chunk) = ([0_u8; 8192], [0_u8; 8192]);
        loop {
            // Do not reap the leader while helpers still hold its output pipes.
            // A timeout can then signal the group without a recycled-PID race.
            if output_closed
                && errors_closed
                && let Some(status) = child.try_wait()?
            {
                return Ok((status, output, errors));
            }
            tokio::select! {
                _ = self.cancelled.cancelled() => bail!("operation cancelled"),
                _ = &mut deadline => bail!("dependency command timed out"),
                notification = exits.recv() => {
                    ensure!(notification.is_some(), "child exit notifications ended");
                }
                read = stdout.read(&mut output_chunk), if !output_closed => {
                    let size = read.context("read dependency stdout")?;
                    output_closed = size == 0;
                    ensure!(output.len() + errors.len() + size <= MAX_OUTPUT, "dependency command exceeded 4 MiB output limit");
                    output.extend_from_slice(&output_chunk[..size]);
                }
                read = stderr.read(&mut error_chunk), if !errors_closed => {
                    let size = read.context("read dependency stderr")?;
                    errors_closed = size == 0;
                    ensure!(output.len() + errors.len() + size <= MAX_OUTPUT, "dependency command exceeded 4 MiB output limit");
                    errors.extend_from_slice(&error_chunk[..size]);
                }
            }
        }
    }
}

async fn terminate_group(child: &mut Child, exits: &mut tokio::signal::unix::Signal) -> Result<()> {
    // Lima's detached hostagent owns a different process group. Keep this leader
    // unreaped until both signals are sent, preventing reuse of its group ID.
    let group = format!("-{}", child.id());
    let _ = signal_group(&group, "-TERM").await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let killed = signal_group(&group, "-KILL").await;
    if killed.is_err() {
        // Last resort for the leader; this does not establish helper cleanup.
        let _ = child.kill();
    }
    let reaped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if child.try_wait()?.is_some() {
                return Ok::<(), anyhow::Error>(());
            }
            ensure!(
                exits.recv().await.is_some(),
                "child exit notifications ended"
            );
        }
    })
    .await
    .context("cancelled dependency did not exit within the cleanup deadline")?;
    reaped?;
    killed.context("could not confirm dependency process-group termination")
}

async fn signal_group(group: &str, signal: &str) -> Result<()> {
    let mut command = tokio::process::Command::new("/bin/kill");
    command
        .args([signal, group])
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let status = tokio::time::timeout(Duration::from_secs(1), command.status())
        .await
        .context("process-group signal command timed out")??;
    ensure!(status.success(), "process-group {signal} failed ({status})");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;
    fn runner() -> Runner {
        Runner {
            cancelled: Cancellation::new(),
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
        runner.cancelled.cancel();
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

    #[test]
    fn exited_leader_cannot_leave_a_pipe_holding_helper_after_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("survived");
        let result = runner().run(
            Command::new("sh")
                .args([
                    "-c",
                    "sh -c 'sleep 1; printf survived > \"$1\"' helper \"$1\" & exit 0",
                    "parent",
                ])
                .arg(&marker),
            Duration::from_millis(100),
        );
        assert!(result.is_err());
        std::thread::sleep(Duration::from_secs(1));
        assert!(!marker.exists(), "orphaned pipe writer survived timeout");
    }

    #[test]
    fn completion_waits_for_exit_even_when_output_is_closed_first() {
        let output = runner()
            .run(
                Command::new("sh").args(["-c", "exec 1>&- 2>&-; sleep 0.05; exit 7"]),
                Duration::from_secs(2),
            )
            .unwrap();
        assert_eq!(output.status.code(), Some(7));
    }

    #[test]
    fn output_limit_covers_both_streams_and_stops_the_writer() {
        let result = runner().run(
            Command::new("sh").args(["-c", "dd if=/dev/zero bs=1048576 count=3 2>/dev/null; dd if=/dev/zero bs=1048576 count=3 >&2 2>/dev/null; sleep 30"]),
            Duration::from_secs(3),
        );
        assert!(format!("{:#}", result.err().unwrap()).contains("output limit"));
    }

    #[test]
    fn active_cancellation_wakes_a_silent_dependency() {
        let runner = runner();
        let cancellation = runner.cancelled.clone();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancellation.cancel();
        });
        let started = Instant::now();
        let result = runner.run(Command::new("sleep").arg("30"), Duration::from_secs(5));
        thread.join().unwrap();
        assert!(format!("{:#}", result.err().unwrap()).contains("cancelled"));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn timeout_forces_termination_when_dependency_ignores_term() {
        let started = Instant::now();
        let result = runner().run(
            Command::new("sh").args(["-c", "trap '' TERM; exec sleep 30"]),
            Duration::from_millis(100),
        );
        let error = format!("{:#}", result.err().unwrap());
        assert!(error.contains("timed out"));
        assert!(!error.contains("cleanup also failed"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
