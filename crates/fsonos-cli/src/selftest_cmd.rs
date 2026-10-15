//! `fsonos selftest`: the live acceptance checks of
//! [`fsonos_core::selftest`] on this host's players, straight over the LAN
//! (no daemon needed). The report is scrubbed for sharing; the exit code is
//! doctor's (0 pass, 6 warnings, 7 a failure).

use clap::Args;
use fsonos_core::doctor::Runner;
use fsonos_core::doctor::lan::LanProbe;
use std::process::ExitCode;
use std::sync::Arc;

use crate::config::GlobalArgs;

#[derive(Debug, Args)]
pub struct SelftestArgs {
    /// Run only the checks whose id starts with this (e.g. `selftest.reads`).
    #[arg(long)]
    only: Option<String>,
}

/// Run the inaudible tier and print the report.
pub fn run(global: &GlobalArgs, args: &SelftestArgs) -> anyhow::Result<ExitCode> {
    let lan = global.network()?.lan;
    let probe = Arc::new(
        LanProbe::new(Arc::clone(&lan), global.seed_addrs()?).with_ssdp_wait(global.wait()),
    );
    let mut runner = Runner::new();
    fsonos_core::selftest::register(&mut runner, lan, &probe);
    let report = crate::doctor::only(runner.run()?, args.only.as_deref());
    if global.json {
        println!("{}", serde_json::to_string_pretty(&report.to_json())?);
    } else {
        print!("{}", report.render_table());
    }
    Ok(ExitCode::from(report.exit_code()))
}
