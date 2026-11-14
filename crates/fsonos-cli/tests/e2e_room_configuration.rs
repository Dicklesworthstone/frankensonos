//! Room aliases and defaults belong to the selected daemon even when the CLI
//! has a different local data directory. Accepted changes affect real control.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

fn command(s: &mut Scenario, step: &str, data: &Path, words: &[&str]) -> Run {
    let path = data.to_str().unwrap();
    let mut args = vec!["--daemon", "--data-dir", path, "--json", "rooms"];
    args.extend_from_slice(words);
    s.cli(step, &args)
}

fn accepted(run: Run) -> Value {
    assert!(run.ok(), "{run:?}");
    serde_json::from_str(&run.stdout).unwrap()
}

fn defaults(s: &mut Scenario, other: &Path) {
    let saved = accepted(command(s, "global-default", other, &["default", "Kitchen"]));
    assert_eq!(saved["configuration"]["default_room"], "Kitchen");
    let revision = saved["configuration"]["revision"].as_str().unwrap();
    let saved = accepted(command(
        s,
        "client-default",
        other,
        &[
            "default",
            "Office",
            "--client",
            "tag:fixture",
            "--expected-revision",
            revision,
        ],
    ));
    assert_eq!(
        saved["configuration"]["client_defaults"]["tag:fixture"],
        "Office"
    );
    let shown = accepted(command(
        s,
        "show-default",
        other,
        &["default", "--client", "tag:fixture"],
    ));
    assert_eq!(shown["client_defaults"]["tag:fixture"], "Office");
    let saved = accepted(command(
        s,
        "clear-client-default",
        other,
        &["default", "--client", "tag:fixture", "--clear"],
    ));
    assert_eq!(saved["configuration"]["default_room"], "Kitchen");
    assert!(
        saved["configuration"]["client_defaults"]
            .as_object()
            .unwrap()
            .is_empty()
    );
}

fn edit_alias(s: &mut Scenario, other: &Path, file: &Path, local: &str) {
    let initial = accepted(command(s, "configuration-read", other, &["configuration"]));
    assert_eq!(initial["aliases"][0]["name"], "old");
    let revision = initial["revision"].as_str().unwrap();
    let saved = accepted(command(
        s,
        "alias-set",
        other,
        &[
            "alias",
            "add",
            "shared-fixture",
            "Kitchen",
            "Office",
            "--expected-revision",
            revision,
        ],
    ));
    assert_eq!(saved["changed"], true);
    assert_ne!(saved["configuration"]["revision"], initial["revision"]);
    let text = std::fs::read_to_string(file).unwrap();
    assert!(text.starts_with("# house notes remain\n"));
    assert!(text.contains("# retained"));
    assert!(text.contains("shared-fixture"));
    assert_eq!(
        std::fs::read_to_string(other.join("aliases.toml")).unwrap(),
        local
    );

    let stale = command(
        s,
        "stale-revision",
        other,
        &[
            "alias",
            "rm",
            "shared-fixture",
            "--expected-revision",
            revision,
        ],
    );
    assert!(!stale.ok());
    assert!(stale.stderr.contains("CONFIG_CHANGED"), "{stale:?}");
    assert_eq!(std::fs::read_to_string(file).unwrap(), text);
}

fn assert_persisted(s: &mut Scenario, expected: &Value) {
    let missing = s.dir().join("no-such-seeds.toml");
    let persisted = accepted(s.cli(
        "direct-offline-read",
        &[
            "--direct",
            "--json",
            "--seeds",
            missing.to_str().unwrap(),
            "rooms",
            "configuration",
        ],
    ));
    assert_eq!(persisted["revision"], expected["revision"]);
    assert_eq!(persisted["default_room"], "Kitchen");
    assert_eq!(persisted["aliases"][0]["name"], "old");
}

#[test]
fn aliases_and_defaults_edit_the_selected_daemon_and_survive_restart() {
    let mut s = Scenario::start("room-configuration");
    s.sim(SimHousehold::standard());
    let data = s.dir().join("data");
    let file = data.join("aliases.toml");
    std::fs::write(
        &file,
        "# house notes remain\n[aliases]\nold = \"Living Room\" # retained\n",
    )
    .unwrap();
    let mut daemon = s.spawn("serve", &["serve", "--http", "127.0.0.1:0"]);
    assert!(
        daemon
            .wait_line("fsonos serve: live", Duration::from_secs(20))
            .is_some()
    );

    // A second CLI host can reuse its authorized daemon record while keeping
    // completely different local configuration. No seeds need to be read.
    let other = s.dir().join("other-host");
    std::fs::create_dir(&other).unwrap();
    std::fs::copy(data.join("daemon.json"), other.join("daemon.json")).unwrap();
    let local = "# this belongs only to the remote CLI host\n[aliases]\nlocal_only = \"Office\"\n";
    std::fs::write(other.join("aliases.toml"), local).unwrap();
    let before = s.sim_handle().unwrap().soap_log().len();
    edit_alias(&mut s, &other, &file, local);
    defaults(&mut s, &other);
    assert!(
        s.sim_handle().unwrap().soap_log()[before..]
            .iter()
            .all(|call| call.action.starts_with("Get") || call.action == "Browse")
    );

    let changed = s.cli(
        "alias-control",
        &[
            "--daemon",
            "--data-dir",
            other.to_str().unwrap(),
            "volume",
            "shared-fixture",
            "18",
        ],
    );
    assert!(changed.ok(), "{changed:?}");
    assert_eq!(get_volume(&s.lan(), s.ip("Kitchen")).unwrap(), 18);
    assert_eq!(get_volume(&s.lan(), s.ip("Office")).unwrap(), 18);
    let removed = accepted(command(
        &mut s,
        "alias-remove",
        &other,
        &["alias", "rm", "shared-fixture"],
    ));
    assert_eq!(
        removed["configuration"]["aliases"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        std::fs::read_to_string(other.join("aliases.toml")).unwrap(),
        local
    );
    drop(daemon);

    assert_persisted(&mut s, &removed["configuration"]);
    s.finish();
}
