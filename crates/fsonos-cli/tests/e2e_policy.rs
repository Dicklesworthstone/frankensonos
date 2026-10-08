//! The house policy on the CLI and MCP: `fsonos policy show` prints the
//! policy.toml in the data directory with the CLI as the caller, and
//! `fsonos policy check` validates it (a bad value names its line, exit 2).
//! The MCP `get_policy` tool answers the same policy for its own caller.

mod e2e;

use e2e::Scenario;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};

const POLICY: &str = "[defaults]\nmax_volume = 60\nmax_step = 15\n\n\
                      [quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = 25\n\n\
                      [rooms.\"Kitchen\"]\nmax_volume = 40\n\n\
                      [clients.\"mcp-stdio\"]\ndeny = [\"dj_start\"]\n";

#[test]
fn the_policy_is_shown_and_checked_alike() {
    let mut s = Scenario::start("policy");
    s.sim(SimHousehold::standard());
    let data = s.dir().join("data");
    let wrote = std::fs::create_dir_all(&data)
        .and_then(|()| std::fs::write(data.join("policy.toml"), POLICY));
    s.check(
        "policy-file",
        "store",
        "a policy.toml",
        wrote.is_ok(),
        format!("{wrote:?}"),
    );

    let run = s.cli("show", &["policy", "show", "--json"]);
    let shown: Value = serde_json::from_str(&run.stdout).unwrap_or(Value::Null);
    s.check(
        "show",
        "cli",
        "fsonos policy show: the file's limits, quiet hours, room cap and client rule, with the CLI uncapped",
        run.code == Some(0)
            && shown["you"] == "cli"
            && shown["you_are_capped"] == false
            && shown["defaults"]["max_volume"] == 60
            && shown["quiet_hours"]["start"] == "22:00"
            && shown["rooms"][0]["room"] == "Kitchen"
            && shown["rooms"][0]["max_volume"] == 40
            && shown["clients"][0]["deny"] == json!(["dj_start"]),
        format!("exit {:?}\n{}{}", run.code, run.stdout, run.stderr),
    );
    let run = s.cli("show-text", &["policy", "show"]);
    s.check(
        "show-text",
        "cli",
        "the text form says the same in lines",
        run.code == Some(0)
            && run
                .stdout
                .contains("Quiet hours: 22:00–07:00, rooms up to 25.")
            && run.stdout.contains("Client mcp-stdio: never dj_start.")
            && run.stdout.contains("You (cli): uncapped."),
        &run.stdout,
    );

    let run = s.cli("check", &["policy", "check"]);
    s.check(
        "check",
        "cli",
        "fsonos policy check: the file is valid, counted",
        run.code == Some(0)
            && run
                .stdout
                .contains("is valid: 1 room(s) and 1 client(s) with rules of their own"),
        format!("exit {:?}\n{}{}", run.code, run.stdout, run.stderr),
    );
    let bad = s.dir().join("bad-policy.toml");
    let _ = std::fs::write(&bad, "[defaults]\nmax_volume = 70\nmax_step = \"lots\"\n");
    let bad_arg = bad.display().to_string();
    let run = s.cli("check-bad", &["policy", "check", "--file", &bad_arg]);
    s.check(
        "check-bad",
        "cli",
        "a bad value is INVALID_ARGUMENT naming its line: exit 2",
        run.code == Some(2)
            && run.stderr.contains("INVALID_ARGUMENT")
            && run.stderr.contains("line 3"),
        format!("exit {:?}\n{}", run.code, run.stderr),
    );

    let mut mcp = s.mcp();
    mcp.initialize();
    let answer = mcp.request(
        "tools/call",
        &json!({ "name": "get_policy", "arguments": {} }),
    );
    let text = answer["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    s.check(
        "get-policy",
        "mcp",
        "get_policy answers the same policy for the MCP caller, whose dj_start is denied and whose volume is capped",
        text.contains("Room Kitchen: up to 40.")
            && text.contains("Client mcp-stdio: never dj_start.")
            && text.contains("You (mcp-stdio): the volume caps apply."),
        &answer,
    );
    s.finish();
}
