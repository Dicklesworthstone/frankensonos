//! An explicitly requested, session-owned Pulse null sink. No default or app moves.

use anyhow::{Context as _, bail, ensure};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

use super::StreamArgs;

mod command;
use command::Pactl;

const OWNER: &str = "frankensonos.stream.id";

/// Resolve the monitor before either encoder or acoustic-capture arguments are built.
/// This is validation only: no Pulse, speaker or microphone I/O.
pub(super) fn prepare(args: &StreamArgs) -> anyhow::Result<StreamArgs> {
    let mut prepared = args.clone();
    if let Some(name) = &args.virtual_output {
        ensure!(
            cfg!(target_os = "linux") && args.format == "pulse",
            "--virtual-output requires Linux --format pulse"
        );
        ensure!(
            args.latency_wav.is_none(),
            "offline WAV measurement needs no output"
        );
        ensure!(
            valid_name(name),
            "use a 1-64 character output name starting with a letter, with only letters, digits, _ or -"
        );
        ensure!(
            args.input == "@DEFAULT_MONITOR@",
            "--virtual-output selects its own monitor; do not also supply --input"
        );
        prepared.input = format!("{name}.monitor");
    }
    Ok(prepared)
}

fn valid_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name.as_bytes()[0].is_ascii_alphabetic()
        && name != "default"
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
}

fn list(bytes: &[u8]) -> anyhow::Result<Vec<Value>> {
    let entries: Vec<Value> = serde_json::from_slice(bytes).context("read pactl JSON device list")?;
    ensure!(
        entries.iter().all(|entry| {
            entry
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
        }),
        "pactl returned a device without a name"
    );
    Ok(entries)
}

fn named<'a>(entries: &'a [Value], name: &str) -> anyhow::Result<Option<&'a Value>> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry["name"].as_str() == Some(name));
    let found = matches.next();
    ensure!(matches.next().is_none(), "Pulse returned duplicate device names");
    Ok(found)
}

fn index(value: &Value) -> anyhow::Result<u32> {
    value
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|n| *n != u32::MAX)
        .context("Pulse output has no valid owning module")
}

/// Pure ownership bookkeeping; tests inject protocol responses, not global PATH changes.
struct Lease {
    name: String,
    token: String,
    module: Option<u32>,
    attempted: bool,
}

impl Lease {
    fn new(name: &str, token: String) -> Self {
        Self {
            name: name.to_owned(),
            token,
            module: None,
            attempted: false,
        }
    }

    fn load_args(&self) -> Vec<String> {
        vec![
            "load-module".into(),
            "module-null-sink".into(),
            format!("sink_name={}", self.name),
            "rate=48000".into(),
            "channels=2".into(),
            "channel_map=front-left,front-right".into(),
            // pactl joins module arguments with spaces. These quotes belong to
            // Pulse's module argument parser, not to a shell.
            format!(
                "sink_properties=\"device.description=FrankenSonos_{} {OWNER}={}\"",
                self.name, self.token
            ),
        ]
    }

    fn marked<'a>(&self, sinks: &'a [Value]) -> anyhow::Result<Option<&'a Value>> {
        let mut matches = sinks.iter().filter(|sink| {
            sink["properties"][OWNER].as_str() == Some(self.token.as_str())
        });
        let found = matches.next();
        ensure!(
            matches.next().is_none(),
            "multiple outputs claim this session; leaving them alone"
        );
        Ok(found)
    }

    fn owned(&self, sink: &Value) -> anyhow::Result<u32> {
        ensure!(
            sink["properties"][OWNER].as_str() == Some(self.token.as_str()),
            "virtual output was replaced or its ownership changed; leaving it alone"
        );
        let module = index(&sink["owner_module"])?;
        ensure!(
            self.module.is_none_or(|old| old == module),
            "virtual output's module changed; leaving it alone"
        );
        Ok(module)
    }

    fn create(
        &mut self,
        mut call: impl FnMut(&[String]) -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<()> {
        let info: Value = serde_json::from_slice(&call(&words(&["--format=json", "info"]))?)?;
        // LC_ALL=C makes pactl's localized yes/no stable. Refuse a configured
        // remote PULSE_SERVER rather than creating a device on another host.
        ensure!(
            info["is_local"].as_str() == Some("yes"),
            "virtual output requires a local Pulse server"
        );
        let sinks = list(&call(&words(&["--format=json", "list", "sinks"]))?)?;
        let sources = list(&call(&words(&["--format=json", "list", "sources"]))?)?;
        let monitor = format!("{}.monitor", self.name);
        ensure!(
            named(&sinks, &self.name)?.is_none() && named(&sources, &monitor)?.is_none(),
            "output or monitor already exists; choose another --virtual-output name"
        );

        // Record intent before dispatch. Failure may follow a successful load;
        // the guard will reconcile by our random ownership property, never retry.
        self.attempted = true;
        let response = call(&self.load_args())?;
        let response = std::str::from_utf8(&response)?.trim();
        ensure!(
            !response.is_empty() && response.bytes().all(|byte| byte.is_ascii_digit()),
            "pactl did not acknowledge one module index; load was not repeated"
        );
        let module: u32 = response.parse().context("invalid Pulse module index")?;
        let sinks = list(&call(&words(&["--format=json", "list", "sinks"]))?)?;
        let sink = named(&sinks, &self.name)?.context("loaded output was not found")?;
        ensure!(
            self.owned(sink)? == module,
            "loaded output and acknowledged module disagree"
        );
        ensure!(
            sink["monitor_source"].as_str() == Some(monitor.as_str()),
            "output has an unexpected monitor source"
        );
        let sources = list(&call(&words(&["--format=json", "list", "sources"]))?)?;
        let source = named(&sources, &monitor)?.context("loaded monitor was not found")?;
        ensure!(
            index(&source["owner_module"])? == module
                && source["monitor_source"].as_str() == Some(self.name.as_str()),
            "monitor does not belong to the created output"
        );
        self.module = Some(module);
        Ok(())
    }

    fn close(
        &mut self,
        mut call: impl FnMut(&[String]) -> anyhow::Result<Vec<u8>>,
    ) -> anyhow::Result<()> {
        if !self.attempted {
            return Ok(());
        }
        // One cleanup attempt, including a lost unload reply. Drop cannot replay it.
        self.attempted = false;
        let sinks = list(&call(&words(&["--format=json", "list", "sinks"]))?)?;
        let Some(sink) = self.marked(&sinks)? else {
            ensure!(
                named(&sinks, &self.name)?.is_none(),
                "virtual output was replaced; leaving the replacement alone"
            );
            return Ok(());
        };
        // Some servers rename a concurrently colliding sink. The unique marker,
        // not a guessed name/index, lets failed startup clean up only its output.
        let module = self.owned(sink)?;
        let unloaded = call(&words(&["unload-module", &module.to_string()]));
        // Accept a lost reply only after a successful read proves our output gone.
        let after = list(&call(&words(&["--format=json", "list", "sinks"]))?)?;
        if self.marked(&after)?.is_some() {
            unloaded?;
            bail!("virtual output still exists after unload; command was not repeated");
        }
        Ok(())
    }
}

fn words(args: &[&str]) -> Vec<String> {
    args.iter().map(|arg| (*arg).to_owned()).collect()
}

pub(super) struct VirtualOutput {
    lease: Lease,
    pactl: Pactl,
}

impl VirtualOutput {
    pub(super) fn start(name: Option<&str>, stop: &AtomicBool) -> anyhow::Result<Option<Self>> {
        let Some(name) = name else {
            return Ok(None);
        };
        ensure!(
            cfg!(target_os = "linux") && valid_name(name),
            "invalid virtual output request"
        );
        let token = fsonos_core::announce::clip::clip_id()?;
        let mut output = Self {
            lease: Lease::new(name, token),
            pactl: Pactl::default(),
        };
        output.lease.create(|args| {
            ensure!(!stop.load(Ordering::Acquire), "virtual output setup cancelled");
            output.pactl.run(args, Some(stop))
        })?;
        eprintln!(
            "fsonos stream: temporary output FrankenSonos_{name} is ready; \
             route only the intended app to {name}. No application or default was moved by fsonos."
        );
        Ok(Some(output))
    }

    /// Do not remove the app's destination when its process could not be reaped.
    /// The owner must inspect the still-loaded output before manual cleanup.
    pub(super) fn retain(&mut self) {
        self.lease.attempted = false;
        eprintln!(
            "fsonos stream: keeping temporary output {} because application cleanup \
             was not confirmed; inspect the application before removing the output",
            self.lease.name
        );
    }

    pub(super) fn shutdown(&mut self) -> anyhow::Result<()> {
        self.lease.close(|args| self.pactl.run(args, None))
    }
}

impl Drop for VirtualOutput {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("fsonos stream: temporary output cleanup was not confirmed: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod application_retention {
    use super::*;

    #[test]
    fn uncertain_application_cleanup_disarms_output_removal_without_io() {
        let mut output = VirtualOutput {
            lease: Lease::new("movie", "synthetic-token".into()),
            pactl: Pactl::default(),
        };
        output.lease.attempted = true;
        output.lease.module = Some(42);
        output.retain();
        // pactl is never launched; Drop cannot retry output removal either.
        assert!(!output.lease.attempted);
        output.shutdown().unwrap();
        output.shutdown().unwrap();
    }
}
