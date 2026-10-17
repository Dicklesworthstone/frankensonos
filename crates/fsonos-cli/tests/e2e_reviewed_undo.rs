//! `fsonos undo --preview` and `--id`: the CLI reviews its own newest
//! undoable action without touching the speakers, then undoes exactly that
//! one. An id that is no longer the newest is refused and restores nothing,
//! so an action that lands in between is never undone by mistake.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::Value;

fn json(run: &Run) -> Value {
    serde_json::from_str(&run.stdout).unwrap_or(Value::Null)
}

#[test]
fn the_cli_previews_its_newest_action_and_undoes_exactly_that_one() {
    let mut s = Scenario::start("reviewed_undo");
    s.sim(SimHousehold::standard());
    let kitchen = s.ip("Kitchen");
    let volume = |s: &Scenario| get_volume(&s.lan(), kitchen).ok();

    let run = s.cli("volume-45", &["volume", "Kitchen", "45"]);
    s.check("volume-45", "cli", "exits 0", run.ok(), &run.stderr);
    let run = s.cli("preview", &["undo", "--preview", "--json"]);
    let reviewed = json(&run)["id"].as_i64();
    s.check(
        "preview",
        "cli",
        "the preview names the CLI's volume change and changes nothing",
        run.ok()
            && reviewed.is_some()
            && json(&run)["intent"]
                .as_str()
                .is_some_and(|i| i.starts_with("set_volume"))
            && volume(&s) == Some(45),
        format!("{}{} {:?}", run.stdout, run.stderr, volume(&s)),
    );

    // Something newer lands after the review: the reviewed id is stale.
    let run = s.cli("volume-30", &["volume", "Kitchen", "30"]);
    s.check("volume-30", "cli", "exits 0", run.ok(), &run.stderr);
    let stale = reviewed.unwrap_or_default().to_string();
    let run = s.cli("stale", &["undo", "--id", &stale]);
    s.check(
        "stale",
        "cli",
        "an id that is no longer the newest is refused (INVALID_ARGUMENT, exit 2) and restores nothing",
        run.code == Some(2) && run.stderr.contains("INVALID_ARGUMENT") && volume(&s) == Some(30),
        format!("{run:?} {:?}", volume(&s)),
    );

    let run = s.cli("preview-again", &["undo", "--preview"]);
    let newest = run
        .stdout
        .strip_prefix('#')
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_default()
        .to_string();
    s.check(
        "preview-again",
        "cli",
        "the text preview shows the newest action and how to undo exactly it",
        run.ok() && run.stdout.contains(&format!("fsonos undo --id {newest}")),
        &run.stdout,
    );
    let run = s.cli("checked", &["undo", "--id", &newest, "--json"]);
    s.check(
        "checked",
        "cli",
        "undoing the reviewed newest action restores Kitchen to 45",
        run.ok()
            && json(&run)["undone"].as_i64().map(|id| id.to_string()) == Some(newest.clone())
            && volume(&s) == Some(45),
        format!("{}{} {:?}", run.stdout, run.stderr, volume(&s)),
    );

    let run = s.cli("both", &["undo", "--preview", "--id", "1"]);
    s.check(
        "both",
        "cli",
        "--preview and --id together are a usage error",
        run.code == Some(2) && run.stderr.contains("cannot be used with"),
        &run.stderr,
    );
    s.finish();
}
