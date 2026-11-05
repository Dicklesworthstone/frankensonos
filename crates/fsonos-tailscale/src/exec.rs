//! Run a short-lived command with a deadline.
//!
//! `std::process` has no wait-with-timeout, so the child's pipes are drained
//! on helper threads (a large `status --json` would otherwise fill the pipe
//! and stall the child) while this thread polls for exit until the deadline.
//! On Linux and macOS the command owns a process group, cleaned up before
//! reaping its leader, so a wrapper cannot leave a helper holding those pipes.
//! Output collection shares the deadline instead of waiting forever for EOF.

use std::io::{self, Read};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);
const DETAIL_MAX: usize = 300;
const BUSY_RETRIES: u32 = 50;
/// How long a killed child's output may take to arrive.
const LATE_OUTPUT: Duration = Duration::from_millis(200);

#[derive(Debug, PartialEq, Eq)]
pub enum ExecError {
    /// The program does not exist (or is not executable).
    NotFound,
    /// It did not finish with complete output before the deadline; a running
    /// command is killed. `output` is captured stdout, then stderr, trimmed.
    TimedOut { output: String },
    /// It could not be run, or exited unsuccessfully; `detail` is stderr (or
    /// the exit status) trimmed for display.
    Failed { detail: String },
}

/// Run `program args…`, returning stdout when it exits successfully within
/// `timeout`.
pub fn run(program: &Path, args: &[&str], timeout: Duration) -> Result<Vec<u8>, ExecError> {
    let spawn = || {
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        command.spawn()
    };
    let mut busy = 0;
    let mut child = loop {
        match spawn() {
            Ok(child) => break child,
            // A just-written executable is briefly "text file busy" while
            // another thread forks with its write handle open; retry.
            Err(e) if e.kind() == io::ErrorKind::ExecutableFileBusy && busy < BUSY_RETRIES => {
                busy += 1;
                thread::sleep(POLL);
            }
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
        }
    };
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let mut child = ChildGuard {
        child,
        completed: false,
    };
    let deadline = Instant::now() + timeout;

    let status = match wait_until(&mut child, deadline) {
        Ok(Some(status)) => status,
        Ok(None) => {
            drop(child);
            // The pipes close with the child; don't wait on a grandchild
            // that kept one open.
            let until = Instant::now() + LATE_OUTPUT;
            let mut printed = receive(&stdout, until).unwrap_or_default();
            printed.extend(receive(&stderr, until).unwrap_or_default());
            return Err(ExecError::TimedOut {
                output: for_display(String::from_utf8_lossy(&printed).trim().to_owned()),
            });
        }
        Err(e) => {
            return Err(ExecError::Failed {
                detail: e.to_string(),
            });
        }
    };
    let stdout = receive(&stdout, deadline).map_err(|_| ExecError::TimedOut {
        output: for_display(
            String::from_utf8_lossy(&stderr.try_recv().unwrap_or_default())
                .trim()
                .to_owned(),
        ),
    })?;
    let stderr = receive(&stderr, deadline).map_err(|_| ExecError::TimedOut {
        output: for_display(String::from_utf8_lossy(&stdout).trim().to_owned()),
    })?;
    if status.success() {
        Ok(stdout)
    } else {
        let detail = String::from_utf8_lossy(&stderr).trim().to_owned();
        let detail = if detail.is_empty() {
            status.to_string()
        } else {
            detail
        };
        Err(ExecError::Failed {
            detail: for_display(detail),
        })
    }
}

/// A shared deadline also bounds reads after the direct child has exited.
fn receive(
    output: &Receiver<Vec<u8>>,
    deadline: Instant,
) -> Result<Vec<u8>, mpsc::RecvTimeoutError> {
    output.recv_timeout(deadline.saturating_duration_since(Instant::now()))
}

/// Reserve the leader's PID until its process group has been cleaned up.
/// Signalling after reaping could otherwise address a reused process id.
struct ChildGuard {
    child: Child,
    completed: bool,
}

impl ChildGuard {
    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
            if waitid(
                WaitId::Pid(Pid::from_child(&self.child)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )?
            .is_none()
            {
                return Ok(None);
            }
            self.kill_group();
            let status = self.child.wait()?;
            self.completed = true;
            Ok(Some(status))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let status = self.child.try_wait()?;
            self.completed = status.is_some();
            Ok(status)
        }
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal, kill_process_group};
            let _ = kill_process_group(Pid::from_child(&self.child), Signal::KILL);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.kill_group();
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// `text`, cut to [`DETAIL_MAX`] bytes on a character boundary.
fn for_display(mut text: String) -> String {
    if text.len() > DETAIL_MAX {
        let cut = (0..=DETAIL_MAX)
            .rev()
            .find(|&i| text.is_char_boundary(i))
            .unwrap_or(0);
        text.truncate(cut);
        text.push('…');
    }
    text
}

fn drain(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut buf);
        }
        let _ = tx.send(buf);
    });
    rx
}

fn wait_until(
    child: &mut ChildGuard,
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
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn completed_commands_do_not_wait_for_inherited_output_pipes() {
        for redirect in ["", " >&2"] {
            let started = Instant::now();
            let script = format!("(sleep 3; printf late{redirect}) & printf ready");
            assert_eq!(
                sh(&script, Duration::from_millis(300)).unwrap(),
                b"ready",
                "a completed probe must not wait for its background helper"
            );
            assert!(started.elapsed() < Duration::from_secs(2));
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn failed_commands_keep_diagnostics_without_waiting_for_helpers() {
        let started = Instant::now();
        assert_eq!(
            sh(
                "(sleep 3; printf late >&2) & printf 'daemon unavailable' >&2; exit 1",
                Duration::from_millis(300),
            ),
            Err(ExecError::Failed {
                detail: "daemon unavailable".into(),
            })
        );
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn a_hung_program_keeps_what_it_printed() {
        // Like `tailscale serve` waiting for HTTPS to be enabled: it prints a
        // URL, then waits.
        let err = sh(
            "echo 'To enable, visit: https://login.example/f/serve'; exec sleep 30",
            Duration::from_millis(500),
        )
        .unwrap_err();
        assert_eq!(
            err,
            ExecError::TimedOut {
                output: "To enable, visit: https://login.example/f/serve".into()
            }
        );
    }

    #[test]
    fn hung_program_is_killed_at_the_deadline() {
        let started = Instant::now();
        let err = sh("exec sleep 30", Duration::from_millis(200)).unwrap_err();
        assert_eq!(
            err,
            ExecError::TimedOut {
                output: String::new()
            }
        );
        assert!(started.elapsed() < Duration::from_secs(10));
    }
}
