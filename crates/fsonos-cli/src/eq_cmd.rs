//! `fsonos eq`: a room's tone, or a change to it (`fsonos eq Kitchen`,
//! `fsonos eq Kitchen --bass 3 --loudness off`). Through a running daemon
//! when one answers (`GET /zones/{room}/eq`, `POST /eq`), else directly; a
//! change is logged, so `fsonos undo` puts the tone back.

use clap::Args;
use fsonos_api::surface::eq::{EqDto, EqRequest, EqSetDto};

use crate::config::GlobalArgs;
use crate::direct::Direct;
use crate::remote::{Daemon, pct};
use crate::{Switch, emit};

#[derive(Debug, Args)]
pub struct EqArgs {
    /// The room.
    pub zone: String,
    /// Bass, from -10 to 10 (0 is flat).
    #[arg(long, allow_negative_numbers = true,
          value_parser = clap::value_parser!(i64).range(-10..=10))]
    pub bass: Option<i64>,
    /// Treble, from -10 to 10 (0 is flat).
    #[arg(long, allow_negative_numbers = true,
          value_parser = clap::value_parser!(i64).range(-10..=10))]
    pub treble: Option<i64>,
    /// Loudness compensation: more bass and treble at low volume.
    #[arg(long, value_enum)]
    pub loudness: Option<Switch>,
}

impl EqArgs {
    /// The change asked for; `None` shows the tone.
    #[must_use]
    pub fn request(&self) -> Option<EqRequest> {
        let change = self.bass.is_some() || self.treble.is_some() || self.loudness.is_some();
        change.then(|| EqRequest {
            zone: self.zone.clone(),
            bass: self.bass,
            treble: self.treble,
            loudness: self.loudness.map(|s| s == Switch::On),
        })
    }
}

/// `fsonos eq`, the CLI finding the speakers itself.
pub fn run(global: &GlobalArgs, args: &EqArgs) -> anyhow::Result<()> {
    let direct = Direct::survey(global)?;
    match args.request() {
        None => emit(global.json, &direct.get_eq(&args.zone)?, EqDto::text),
        Some(req) => emit(global.json, &direct.set_eq(&req)?, set_text),
    }
}

/// `fsonos eq` through the daemon.
pub fn through(daemon: &Daemon, json: bool, args: &EqArgs) -> anyhow::Result<()> {
    match args.request() {
        None => {
            let eq: EqDto = daemon.get(&format!("/zones/{}/eq", pct(&args.zone)))?;
            emit(json, &eq, EqDto::text)
        }
        Some(req) => {
            let set: EqSetDto = daemon.post("/eq", &req)?;
            emit(json, &set, set_text)
        }
    }
}

fn set_text(set: &EqSetDto) -> String {
    format!("{}\n", set.done)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: EqArgs,
    }

    fn parse(argv: &[&str]) -> Result<EqArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("eq").chain(argv.iter().copied())).map(|c| c.args)
    }

    #[test]
    fn no_level_shows_the_tone_and_a_level_changes_only_itself() {
        assert_eq!(parse(&["Kitchen"]).unwrap().request(), None);
        let req = parse(&["Kitchen", "--bass", "-3", "--loudness", "off"])
            .unwrap()
            .request()
            .unwrap();
        assert_eq!(
            req,
            EqRequest {
                zone: "Kitchen".into(),
                bass: Some(-3),
                treble: None,
                loudness: Some(false),
            }
        );
        assert!(parse(&["Kitchen", "--treble", "11"]).is_err());
    }
}
