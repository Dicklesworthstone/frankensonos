//! Run a short-lived command with a deadline.
//!
//! `std::process` has no wait-with-timeout, so the child's pipes are drained
//! on helper threads (a large `status --json` would otherwise fill the pipe
//! and stall the child) while this thread polls for exit until the deadline,
//! then kills the child.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);
const DETAIL_MAX: usize = 300;

#[derive(Debug, PartialEq, Eq)]
pub enum ExecError {
    /// The program does not exist (or is not executable).
    NotFound,
    /// It did not exit before the deadline and was killed.
    TimedOut,
    /// It could not be run, or exited unsuccessfully; `detail` is stderr (or
    /// the exit status) trimmed for display.
    Failed { detail: String },
}

/// Run `program args…`, returning stdout when it exits successfully within
/// `timeout`.
pub fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, ExecError> {
    let mut child = match Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::PermissionDenied
            ) =>
        {
            return Err(ExecError::NotFound);
        }
        Err(e) => {
            return Err(ExecError::Failed {
                detail: e.to_string(),
            });
        }
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());

    let status = match wait_until(&mut child, Instant::now() + timeout) {
        Ok(Some(status)) => status,
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExecError::TimedOut);
        }
        Err(e) => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(ExecError::Failed {
                detail: e.to_string(),
            });
        }
    };
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    if status.success() {
        Ok(stdout)
    } else {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let mut detail = if detail.is_empty() {
            status.to_string()
        } else {
            detail
        };
        if detail.len() > DETAIL_MAX {
            let cut = (0..=DETAIL_MAX)
                .rev()
                .find(|&i| detail.is_char_boundary(i))
                .unwrap_or(0);
            detail.truncate(cut);
            detail.push('…');
        }
        Err(ExecError::Failed { detail })
    }
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    })
}

fn wait_until(
    child: &mut Child,
    deadline: Instant,
) -> io::Result<Option<std::process::ExitStatus>> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Some(status));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        thread::sleep(POLL);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Result<Vec<u8>, ExecError> {
        run(Path::new("/bin/sh"), &["-c", script], timeout)
    }

    #[test]
    fn returns_stdout_on_success() {
        assert_eq!(sh("printf ok", Duration::from_secs(5)).unwrap(), b"ok");
    }

    #[test]
    fn drains_output_larger_than_a_pipe_buffer() {
        // 1 MiB: far past the 64 KiB pipe buffer, so an undrained pipe would
        // stall the child until the deadline.
        let out = sh("head -c 1048576 /dev/zero", Duration::from_secs(10)).unwrap();
        assert_eq!(out.len(), 1_048_576);
    }

    #[test]
    fn missing_program_is_not_found() {
        let err = run(
            Path::new("/nonexistent/fsonos-test/tailscale"),
            &[],
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert_eq!(err, ExecError::NotFound);
    }

    #[test]
    fn failure_carries_stderr() {
        let err = sh(
            "echo 'daemon not running' >&2; exit 1",
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ExecError::Failed {
                detail: "daemon not running".into()
            }
        );
    }

    #[test]
    fn hung_program_is_killed_at_the_deadline() {
        let started = Instant::now();
        let err = sh("exec sleep 30", Duration::from_millis(200)).unwrap_err();
        assert_eq!(err, ExecError::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
