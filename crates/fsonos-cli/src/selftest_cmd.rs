//! `fsonos selftest`: the live acceptance checks of
//! [`fsonos_core::selftest`] on this host's players, straight over the LAN
//! (no daemon needed). The report is scrubbed for sharing; the exit code is
//! doctor's (0 pass, 6 warnings, 7 a failure). A full run (no `--only`) is
//! kept in the data directory, where `fsonos doctor` reports it
//! (`selftest.last`). `--transcript` also writes every network exchange of
//! the run, raw, for the owner's own debugging: unlike the report, it is
//! not scrubbed, so it stays local.
//!
//! `--audible --room R [--room R2]` runs the audible tier instead
//! ([`fsonos_core::selftest::audible`]), after asking: steps that make sound
//! in those rooms, at `--max-volume` at most, every room put back at the end,
//! also after Ctrl-C (a second Ctrl-C quits at once).

use anyhow::{Context as _, bail};
use clap::Args;
use fsonos_core::doctor::lan::LanProbe;
use fsonos_core::doctor::{Report, Runner};
use fsonos_core::selftest::audible::{self, MAX_VOLUME, Plan};
use fsonos_proto::net::Exchange;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;
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
    /// Run the audible tier instead, with you listening: steps that make
    /// sound in --room. Every room is put back at the end, also after Ctrl-C.
    #[arg(long)]
    audible: bool,
    /// A room for --audible; two of one household also group, and the music
    /// moves between them.
    #[arg(long, value_name = "ROOM")]
    room: Vec<String>,
    /// The loudest level --audible sets [default: 20, the most without --loud].
    #[arg(long, value_name = "LEVEL")]
    max_volume: Option<u8>,
    /// Let --max-volume go above 20.
    #[arg(long)]
    loud: bool,
    /// Start --audible without asking.
    #[arg(long)]
    yes: bool,
}

/// Run the inaudible tier (or, with `--audible`, the audible one) and print
/// the report.
pub fn run(global: &GlobalArgs, args: &SelftestArgs) -> anyhow::Result<ExitCode> {
    if args.audible {
        return run_audible(global, args);
    }
    if !args.room.is_empty() || args.max_volume.is_some() || args.loud || args.yes {
        bail!("--room, --max-volume, --loud and --yes go with --audible");
    }
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
    show(global, &report)
}

fn show(global: &GlobalArgs, report: &Report) -> anyhow::Result<ExitCode> {
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_table());
    }
    Ok(ExitCode::from(report.exit_code()))
}

/// `--audible`: say what will sound and ask, then run the audible tier.
/// Ctrl-C ends its steps early and the rooms are put back; a second one
/// quits at once.
fn run_audible(global: &GlobalArgs, args: &SelftestArgs) -> anyhow::Result<ExitCode> {
    if args.room.is_empty() || args.room.len() > 2 {
        bail!("--audible takes one --room, or two");
    }
    let cap = args.max_volume.unwrap_or(MAX_VOLUME);
    if cap > MAX_VOLUME && !args.loud {
        bail!("--max-volume above {MAX_VOLUME} needs --loud: these steps play while you listen");
    }
    if cap < 2 {
        bail!("--max-volume must be 2 or more, so a fade has somewhere to go");
    }
    eprintln!(
        "fsonos selftest --audible: sound in {} (and any room grouped with them) at volume \
         {cap} at most. A favorite plays and a chime over it; the volume, the tone and a fade \
         change; a sleep timer is set and cancelled{}. Every room is put back at the end, also after Ctrl-C \
         (a second Ctrl-C quits at once and leaves them as they are).",
        args.room.join(" and "),
        if args.room.len() == 2 {
            "; the rooms group, and the music moves between them"
        } else {
            ""
        }
    );
    if !args.yes {
        eprint!("Type yes to start: ");
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim() != "yes" {
            bail!("not started");
        }
    }
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register_conditional_shutdown(signal, 130, Arc::clone(&stop))
            .and_then(|_| signal_hook::flag::register(signal, Arc::clone(&stop)))
            .context("install the Ctrl-C handler")?;
    }
    let lan = global.network()?.lan;
    let seeds = global.seed_addrs()?;
    let probe =
        Arc::new(LanProbe::new(Arc::clone(&lan), seeds.clone()).with_ssdp_wait(global.wait()));
    // A chime is served from this process, as `fsonos chime` serves one.
    let chimes = global.clone();
    let announce: audible::Announcer = Arc::new(move |room: &str, level: u8| {
        let request = fsonos_api::surface::announce::AnnounceRequest {
            chime: Some("bell".to_owned()),
            rooms: vec![room.to_owned()],
            volume: Some(i64::from(level)),
            ..Default::default()
        };
        crate::announce_cmd::run(&chimes, &request)
            .map(|announced| announced.clean)
            .map_err(|failure| failure.detail)
    });
    let plan = Plan {
        rooms: args.room.clone(),
        max_volume: cap,
        seeds,
        wait: global.wait(),
        announce: Some(announce),
    };
    let mut runner = Runner::new();
    audible::register(
        &mut runner,
        lan,
        &probe,
        plan,
        stop,
        Box::new(|line| eprintln!("fsonos selftest: {line}")),
    );
    show(global, &runner.run()?)
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
