//! Persist a speech or chime recipe with the immediate command's options.

use clap::{Parser, Subcommand};
use fsonos_api::Failure;
use fsonos_api::surface::schedules::ScheduleRequest;

#[derive(Parser)]
#[command(
    name = "scheduled announcement",
    disable_help_flag = true,
    disable_version_flag = true
)]
struct Arguments {
    #[command(subcommand)]
    source: Source,
}

#[derive(Subcommand)]
enum Source {
    Say(crate::announce_cmd::SayArgs),
    Chime(crate::announce_cmd::ChimeArgs),
}

pub(super) fn request(when: &str, words: &[String]) -> Result<ScheduleRequest, Failure> {
    let args = Arguments::try_parse_from(
        std::iter::once("scheduled announcement").chain(words.iter().map(String::as_str)),
    )
    .map_err(|error| {
        Failure::invalid(error.to_string()).with_hint(
            "Use say 'Time to leave' --rooms Kitchen,Office --volume 25, or chime bell --rooms Office.",
        )
    })?;
    let announcement = match args.source {
        Source::Say(args) => args.request(),
        Source::Chime(args) => args.request(),
    };
    let request = ScheduleRequest {
        when: when.to_owned(),
        action: "announce".into(),
        announcement: Some(announcement),
        ..ScheduleRequest::default()
    };
    request.validate()?;
    Ok(request)
}

#[cfg(test)]
mod tests;
