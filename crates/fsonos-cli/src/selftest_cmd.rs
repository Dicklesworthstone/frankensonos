//! `fsonos selftest`: the live acceptance checks of
//! [`fsonos_core::selftest`] on this host's players, straight over the LAN
//! (no daemon needed). The report is scrubbed for sharing; the exit code is
//! doctor's (0 pass, 6 warnings, 7 a failure). A full run (no `--only`) is
//! kept in the data directory, where `fsonos doctor` reports it
//! (`selftest.last`). `--transcript` also writes every network exchange of
//! the run, raw, for the owner's own debugging: unlike the report, it is
//! not scrubbed, so it stays local.

use anyhow::Context as _;
use clap::Args;
use fsonos_core::doctor::lan::LanProbe;
use fsonos_core::doctor::{Report, Runner};
use fsonos_proto::net::Exchange;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, PoisonError};

use crate::config::GlobalArgs;

#[derive(Debug, Args)]
pub struct SelftestArgs {
    /// Run only the checks whose id starts with this (e.g. `selftest.reads`).
    #[arg(long)]
    only: Option<String>,
    /// Also write every network exchange of the run (SOAP, description
    /// reads, GENA) as JSON lines to PATH [default:
    /// selftest-transcript-<time>.jsonl in the data directory]. It is raw:
    /// player addresses, ids and room names appear as sent, so keep it
    /// local.
    #[arg(long, value_name = "PATH", num_args = 0..=1)]
    #[allow(clippy::option_option)] // absent, bare (the default path), or a path
    transcript: Option<Option<PathBuf>>,
}

/// Run the inaudible tier and print the report.
pub fn run(global: &GlobalArgs, args: &SelftestArgs) -> anyhow::Result<ExitCode> {
    let lan = global.network()?.lan;
    let transcript = args
        .transcript
        .as_ref()
        .map(|path| record(global, &lan, path.as_deref()))
        .transpose()?;
    let probe = Arc::new(
        LanProbe::new(Arc::clone(&lan), global.seed_addrs()?).with_ssdp_wait(global.wait()),
    );
    let mut runner = Runner::new();
    fsonos_core::selftest::register(&mut runner, Arc::clone(&lan), &probe);
    let report = crate::doctor::only(runner.run()?, args.only.as_deref());
    if let Some(path) = transcript {
        lan.untap();
        eprintln!(
            "fsonos selftest: transcript in {} (raw, not scrubbed: keep it local)",
            path.display()
        );
    }
    if args.only.is_none() {
        keep(global, &report);
    }
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_table());
    }
    Ok(ExitCode::from(report.exit_code()))
}

/// Tap `lan` into a new transcript file at `path` (none: the data
/// directory's default); the path.
fn record(
    global: &GlobalArgs,
    lan: &fsonos_proto::net::Lan,
    path: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    let path = if let Some(path) = path {
        path.to_path_buf()
    } else {
        let dir = global
            .data_dir()
            .context("no data directory: give --transcript a path")?;
        dir.join(format!(
            "selftest-transcript-{}.jsonl",
            crate::doctor::selftest_last::unix_now()
        ))
    };
    let file = std::fs::File::create(&path)
        .with_context(|| format!("create the transcript {}", path.display()))?;
    let file = Mutex::new(file);
    lan.tap(Box::new(move |x: &Exchange| {
        let line = transcript_line(x);
        let mut file = file.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = writeln!(file, "{line}");
    }));
    Ok(path)
}

/// One exchange as a JSON line.
fn transcript_line(x: &Exchange) -> String {
    let answered_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let (status, reply_headers, response, error) = match &x.reply {
        Ok(answer) => (
            Some(answer.status),
            answer.headers.clone(),
            answer.body.as_str(),
            None,
        ),
        Err(why) => (None, Vec::new(), "", Some(why.as_str())),
    };
    serde_json::json!({
        "answered_at_ms": answered_at_ms,
        "method": x.method,
        "url": x.url,
        "headers": x.headers,
        "request": x.request,
        "status": status,
        "reply_headers": reply_headers,
        "response": response,
        "error": error,
        "elapsed_ms": u64::try_from(x.elapsed.as_millis()).unwrap_or(u64::MAX),
    })
    .to_string()
}

/// Keep a full run for `fsonos doctor`; a run that cannot be kept is still
/// reported.
fn keep(global: &GlobalArgs, report: &Report) {
    use crate::doctor::selftest_last::{save, unix_now};
    let Some(dir) = global.data_dir() else {
        return;
    };
    if let Err(e) = save(&dir, report, unix_now()) {
        eprintln!("fsonos selftest: not kept for `fsonos doctor`: {e}");
    }
}
