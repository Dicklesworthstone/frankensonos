//! One directly launched pactl, bounded output/deadline, no write retry or shell.

use anyhow::{Context as _, bail, ensure};
use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const OUTPUT_LIMIT: usize = 1_048_576;

pub(super) struct Pactl {
    executable: String,
    timeout: Duration,
}

impl Default for Pactl {
    fn default() -> Self {
        Self { executable: "pactl".into(), timeout: Duration::from_secs(5) }
    }
}

struct Process {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() { let _ = reader.join(); }
    }
}

impl Pactl {
    pub(super) fn run(&self, args: &[String], stop: Option<&AtomicBool>) -> anyhow::Result<Vec<u8>> {
        ensure!(!stop.is_some_and(|flag| flag.load(Ordering::Acquire)), "pactl cancelled before dispatch");
        let started = Instant::now();
        let mut process = Process {
            child: Command::new(&self.executable).args(args).env("LC_ALL", "C")
                .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::inherit())
                .spawn().context("start pactl (install PulseAudio client tools)")?,
            reader: None,
        };
        let stdout = process.child.stdout.take().context("pactl has no output pipe")?;
        let (send, receive) = mpsc::sync_channel(1);
        process.reader = Some(thread::Builder::new().name("fsonos-pactl".into()).spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.take((OUTPUT_LIMIT + 1) as u64).read_to_end(&mut bytes).map(|_| bytes);
            let _ = send.send(result);
        })?);
        let mut bytes = None;
        loop {
            ensure!(!stop.is_some_and(|flag| flag.load(Ordering::Acquire)), "pactl cancelled; command not repeated");
            ensure!(started.elapsed() < self.timeout, "pactl deadline exceeded; command not repeated");
            if bytes.is_none() {
                match receive.try_recv() {
                    Ok(result) => {
                        let output = result.context("read pactl output")?;
                        ensure!(output.len() <= OUTPUT_LIMIT, "pactl output limit exceeded");
                        bytes = Some(output);
                    }
                    Err(mpsc::TryRecvError::Empty) => {}
                    Err(mpsc::TryRecvError::Disconnected) => bail!("pactl reader ended unexpectedly"),
                }
            }
            if let Some(status) = process.child.try_wait()? {
                ensure!(status.success(), "pactl failed ({status}); command not repeated");
                if let Some(bytes) = bytes.take() { return Ok(bytes); }
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn runner() -> Pactl {
        Pactl { executable: "/bin/sh".into(), timeout: Duration::from_millis(200) }
    }

    // Shells here are deliberate process fixtures. Production launches pactl
    // itself, with only the fixed argument forms in the parent module.
    fn args(script: &str) -> Vec<String> { vec!["-c".into(), script.into()] }

    #[test]
    fn real_child_output_and_failure_are_propagated() {
        let mut pactl = runner();
        pactl.timeout = Duration::from_secs(3);
        assert_eq!(pactl.run(&args("printf '42\\n'"), None).unwrap(), b"42\n");
        assert!(pactl.run(&args("exit 7"), None).unwrap_err().to_string().contains("failed"));
    }

    #[test]
    fn deadline_terminates_and_reaps_a_direct_child() {
        let started = Instant::now();
        let error = runner().run(&args("exec sleep 30"), None).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn cancellation_before_dispatch_and_during_a_command_are_bounded() {
        let stop = Arc::new(AtomicBool::new(true));
        let missing = Pactl { executable: "/does-not-exist".into(), ..runner() };
        assert!(missing.run(&[], Some(&stop)).unwrap_err().to_string().contains("before dispatch"));
        stop.store(false, Ordering::Release);
        let cancel = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::Release);
        });
        let error = runner().run(&args("exec sleep 30"), Some(&stop)).unwrap_err();
        worker.join().unwrap();
        assert!(error.to_string().contains("cancelled"));
    }

    #[test]
    fn oversized_stdout_cannot_turn_into_a_truncated_success() {
        let mut pactl = runner();
        pactl.timeout = Duration::from_secs(3);
        let error = pactl.run(&args("exec head -c 1048577 /dev/zero"), None).unwrap_err();
        assert!(error.to_string().contains("output limit"));
    }
}
