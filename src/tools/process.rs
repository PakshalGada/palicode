//! Child-process execution with output capture, truncation, and group kill.

use anyhow::{Context, Result};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

#[derive(Debug)]
pub struct ProcessOutput {
    pub status: Option<ExitStatus>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl ProcessOutput {
    pub fn exit_code(&self) -> Option<i32> {
        self.status.and_then(|status| status.code())
    }

    pub fn success(&self) -> bool {
        !self.timed_out && self.status.map(|status| status.success()).unwrap_or(false)
    }
}

#[derive(Default)]
struct Sink {
    data: Vec<u8>,
    total: usize,
}

/// Run a command, capturing stdout/stderr up to `max_bytes` per stream.
///
/// The child is placed in its own process group on Unix so a timeout kills
/// grandchildren too.
pub async fn run(mut command: Command, limit: Duration, max_bytes: usize) -> Result<ProcessOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let mut child = command.spawn().with_context(|| "cannot spawn command")?;
    let pid = child.id();

    let stdout = child
        .stdout
        .take()
        .context("cannot capture command stdout")?;
    let stderr = child
        .stderr
        .take()
        .context("cannot capture command stderr")?;

    let out_sink = Arc::new(Mutex::new(Sink::default()));
    let err_sink = Arc::new(Mutex::new(Sink::default()));
    let out_task = tokio::spawn(drain(stdout, out_sink.clone(), max_bytes));
    let err_task = tokio::spawn(drain(stderr, err_sink.clone(), max_bytes));

    let (status, timed_out) = match timeout(limit, child.wait()).await {
        Ok(result) => (Some(result.context("cannot wait for command")?), false),
        Err(_) => {
            kill_tree(&mut child, pid).await;
            (None, true)
        }
    };

    // Give readers a moment to finish; the buffers are shared, so a stuck
    // grandchild holding a pipe open cannot lose what was already read.
    let _ = timeout(Duration::from_secs(3), async {
        let _ = out_task.await;
        let _ = err_task.await;
    })
    .await;

    let out = out_sink.lock().unwrap();
    let err = err_sink.lock().unwrap();

    Ok(ProcessOutput {
        status,
        timed_out,
        stdout: String::from_utf8_lossy(&out.data).to_string(),
        stderr: String::from_utf8_lossy(&err.data).to_string(),
        stdout_truncated: out.total > out.data.len(),
        stderr_truncated: err.total > err.data.len(),
    })
}

async fn drain<R: AsyncRead + Unpin>(mut reader: R, sink: Arc<Mutex<Sink>>, limit: usize) {
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(read) => {
                let mut sink = sink.lock().unwrap();
                sink.total += read;
                if sink.data.len() < limit {
                    let take = (limit - sink.data.len()).min(read);
                    sink.data.extend_from_slice(&chunk[..take]);
                }
            }
            Err(_) => break,
        }
    }
}

async fn kill_tree(child: &mut tokio::process::Child, pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // Negative pid targets the whole process group.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let _ = child.start_kill();
    let _ = timeout(Duration::from_secs(5), child.wait()).await;
    #[cfg(not(unix))]
    let _ = pid;
}

/// Fail early when a binary is not on PATH (used by tests to skip git).
#[cfg(test)]
pub fn command_exists(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn captures_output_and_status() {
        let mut command = Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("printf 'hello'; printf 'oops' >&2; exit 3");
        let output = run(command, Duration::from_secs(5), 1_000).await.unwrap();
        assert_eq!(output.stdout, "hello");
        assert_eq!(output.stderr, "oops");
        assert_eq!(output.exit_code(), Some(3));
        assert!(!output.timed_out);
    }

    #[tokio::test]
    async fn kills_on_timeout() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("sleep 10");
        let output = run(command, Duration::from_millis(150), 1_000)
            .await
            .unwrap();
        assert!(output.timed_out);
        assert!(output.status.is_none());
    }

    #[tokio::test]
    async fn truncates_large_output() {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg("printf 'aaaaaaaaaaaaaaaaaaaa'");
        let output = run(command, Duration::from_secs(5), 5).await.unwrap();
        assert_eq!(output.stdout, "aaaaa");
        assert!(output.stdout_truncated);
    }
}
