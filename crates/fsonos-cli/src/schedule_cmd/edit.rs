//! Replace one reviewed routine through the selected daemon or local store.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::schedules::{ScheduleDto, UpdateScheduleRequest};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;

#[derive(Args)]
pub struct ScheduleEditArgs {
    /// Existing schedule id from schedule list.
    #[arg(value_parser = clap::value_parser!(i64).range(1..))]
    pub id: i64,
    /// Exact revision from --json schedule list; required before the action words.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
    /// New time: in 45m, an RFC 3339 time, daily 22:30, weekdays 07:30, etc.
    pub when: String,
    /// Complete replacement action, using the same syntax as schedule add.
    #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
    pub action: Vec<String>,
}

impl ScheduleEditArgs {
    fn request(&self) -> Result<UpdateScheduleRequest, Failure> {
        let request = UpdateScheduleRequest {
            id: self.id,
            expected_revision: self.expected_revision.clone(),
            schedule: super::schedule_request(&self.when, &self.action)?,
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &ScheduleEditArgs) -> anyhow::Result<()> {
    // Reject invalid actions and missing proofs before discovery or daemon I/O.
    let request = args.request()?;
    let result: ScheduleDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/schedules/update", &request)?
    } else {
        super::surface(global)?.update_schedule(&Client::Cli, &request)?
    };
    crate::emit(global.json, &result, |schedule| {
        format!("updated {}\n", schedule.line())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    fn parse(words: &[&str]) -> ScheduleEditArgs {
        let cli = Cli::try_parse_from(words).unwrap();
        let Command::Schedule {
            action: super::super::ScheduleCommand::Edit(args),
        } = cli.command
        else {
            panic!("expected schedule edit")
        };
        args
    }

    #[test]
    fn edit_reuses_every_existing_action_parser_and_preserves_the_revision() {
        for (action, kind) in [
            (
                vec!["dj", "start", "Den@S2", "--genre", "jazz", "--for", "2h"],
                "dj_start",
            ),
            (vec!["pause", "Den@S2", "--fade", "30s"], "pause"),
            (vec!["volume", "Den@S2", "17"], "volume"),
            (vec!["scene", "Evening"], "apply_scene"),
            (
                vec!["say", "Time for tea", "--rooms", "Den@S2", "--volume", "20"],
                "announce",
            ),
            (vec!["chime", "bell", "--rooms", "Den@S2"], "announce"),
        ] {
            let mut words = vec![
                "fsonos",
                "--json",
                "--daemon",
                "schedule",
                "edit",
                "7",
                "--expected-revision",
                "schedule-v1-observed",
                "weekdays 08:00",
            ];
            words.extend(action);
            let req = parse(&words).request().unwrap();
            assert_eq!(req.id, 7);
            assert_eq!(req.expected_revision, "schedule-v1-observed");
            assert_eq!(req.schedule.when, "weekdays 08:00");
            assert_eq!(req.schedule.action, kind);
        }
    }

    #[test]
    fn edit_requires_identity_revision_and_a_valid_complete_action() {
        assert!(
            Cli::try_parse_from([
                "fsonos",
                "schedule",
                "edit",
                "7",
                "daily 08:00",
                "pause",
                "Den"
            ])
            .is_err()
        );
        for id in ["0", "-1", "9223372036854775808"] {
            assert!(
                Cli::try_parse_from([
                    "fsonos",
                    "schedule",
                    "edit",
                    id,
                    "--expected-revision",
                    "r",
                    "daily 08:00",
                    "pause",
                    "Den"
                ])
                .is_err()
            );
        }
        for (revision, action) in [
            (" ", vec!["pause", "Den"]),
            ("r", vec!["volume", "Den", "101"]),
            ("r", vec!["unknown", "Den"]),
        ] {
            let mut words = vec![
                "fsonos",
                "--direct",
                "schedule",
                "edit",
                "7",
                "--expected-revision",
                revision,
                "daily 08:00",
            ];
            words.extend(action);
            assert!(parse(&words).request().is_err());
        }
    }
}
