//! A selected daemon owns an agent's saved configuration and future work.
//! The stdio process has a separate, populated store and a different policy:
//! neither can substitute for the daemon's scenes, schedules or identity.

mod e2e;

use e2e::{Daemon, McpSession, Scenario, http};
use fsonos_proto::control::get_volume;
use fsonos_sim::SimHousehold;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const SCENE: &str = "Agent Evening";
const SCENE_PATH: &str = "/scenes/Agent%20Evening";
const LOCAL_SCENE: &str = "Local Only";
const START: &str = "2030-01-02T12:00:00+00:00";
const DUE: &str = "2030-01-02T12:06:00+00:00";
const DAEMON_POLICY: &str = r#"
[defaults]
max_volume = 31
max_step = 100
fade_secs = 0

[clients."mcp-stdio"]
allow = ["read", "house-config", "volume", "apply_scene"]
"#;
const DENIED_POLICY: &str = r#"
[defaults]
max_volume = 31
max_step = 100
fade_secs = 0

[clients."mcp-stdio"]
allow = ["read", "delete_scene", "pause_schedule", "resume_schedule", "remove_schedule"]
"#;
const LOCAL_POLICY: &str = r#"
[defaults]
max_volume = 97
max_step = 100
fade_secs = 0

[clients."mcp-stdio"]
capped = false
"#;

struct Fixture {
    scenario: Scenario,
    daemon: Option<Daemon>,
    api: String,
    client_dir: PathBuf,
    clock: PathBuf,
    local_scenes: Value,
    local_schedules: Value,
}

impl Fixture {
    fn new(name: &str, policy: &str) -> Self {
        let mut scenario = Scenario::start(name);
        scenario.sim(SimHousehold::standard());
        std::fs::write(scenario.dir().join("data/policy.toml"), policy).unwrap();
        let client_dir = scenario.dir().join("agent-data");
        std::fs::create_dir_all(&client_dir).unwrap();
        std::fs::write(client_dir.join("policy.toml"), LOCAL_POLICY).unwrap();
        let clock = scenario.dir().join("clock");
        std::fs::write(&clock, START).unwrap();
        let clock_arg = clock.display().to_string();
        let mut daemon = scenario.spawn(
            "serve",
            &[
                "serve",
                "--http",
                "127.0.0.1:0",
                "--mcp-http",
                "127.0.0.1:0",
                "--clock-file",
                &clock_arg,
            ],
        );
        let ready = daemon
            .wait_line("fsonos serve: ready", Duration::from_secs(20))
            .expect("serve announces its listeners");
        let api = ready
            .split_whitespace()
            .find_map(|word| word.strip_prefix("http=http://"))
            .expect("serve names its HTTP listener")
            .to_owned();
        assert!(api.starts_with("127.0.0.1:"));
        assert!(
            daemon
                .wait_line("fsonos serve: live", Duration::from_secs(20))
                .is_some(),
            "{}",
            daemon.seen.join("\n")
        );
        let mut fixture = Self {
            scenario,
            daemon: Some(daemon),
            api,
            client_dir,
            clock,
            local_scenes: Value::Null,
            local_schedules: Value::Null,
        };
        fixture.seed_local_store();
        // Share only the fixture's generated connection record. Both commands
        // use the same confined routes; their databases and policy files differ.
        std::fs::copy(
            fixture.scenario.dir().join("data/daemon.json"),
            fixture.client_dir.join("daemon.json"),
        )
        .unwrap();
        fixture
    }

    fn local(&mut self, step: &str, args: &[&str]) -> Value {
        let dir = self.client_dir.display().to_string();
        let mut command = vec!["--direct", "--json", "--data-dir", &dir];
        command.extend_from_slice(args);
        let run = self.scenario.cli(step, &command);
        assert!(run.ok(), "{run:?}");
        serde_json::from_str(&run.stdout).unwrap()
    }

    fn seed_local_store(&mut self) {
        self.local("local-scene", &["scene", "save", LOCAL_SCENE]);
        self.local(
            "local-schedule",
            &["schedule", "add", "in 24h", "volume", "Kitchen", "12"],
        );
        self.local_scenes = self.local("local-scenes-before", &["scene", "list"]);
        self.local_schedules = self.local("local-schedules-before", &["schedule", "list"]);
        assert_eq!(self.local_scenes.as_array().unwrap().len(), 1);
        assert_eq!(self.local_schedules.as_array().unwrap().len(), 1);
    }

    fn session(&mut self) -> McpSession {
        let dir = self.client_dir.display().to_string();
        let mut session = self.scenario.mcp_with(&["--data-dir", &dir, "--daemon"]);
        let initialized = session.initialize();
        assert_eq!(
            initialized["result"]["protocolVersion"], "2024-11-05",
            "{initialized}"
        );
        session
    }

    fn get(&self, path: &str) -> Value {
        let (status, _, body) = http(&self.api, "GET", path, &[], "").unwrap();
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    fn schedule(&self, id: i64) -> Value {
        self.get("/schedules")
            .as_array()
            .unwrap()
            .iter()
            .find(|schedule| schedule["id"] == id)
            .expect("the daemon persists the schedule")
            .clone()
    }

    fn volume(&self) -> u8 {
        get_volume(&self.scenario.lan(), self.scenario.ip("Kitchen")).unwrap()
    }

    fn local_unchanged(&mut self) {
        let scenes = self.local("local-scenes-after", &["scene", "list"]);
        let schedules = self.local("local-schedules-after", &["schedule", "list"]);
        assert_eq!(scenes, self.local_scenes);
        assert_eq!(schedules, self.local_schedules);
    }

    fn stop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let code = daemon.interrupt(Duration::from_secs(10));
            self.scenario.check(
                "stop",
                "daemon",
                "the isolated daemon stops cleanly",
                code == Some(0),
                format!("exit {code:?}"),
            );
            assert_eq!(code, Some(0));
        }
    }

    fn finish(mut self) {
        self.stop();
        self.scenario.finish();
    }
}

fn tool(session: &mut McpSession, name: &str, arguments: Value) -> Value {
    let mut params = json!({ "name": name });
    params["arguments"] = arguments;
    let reply = session.request("tools/call", &params);
    assert!(reply.get("error").is_none(), "{reply}");
    assert!(reply["result"].is_object(), "{reply}");
    reply["result"].clone()
}

fn text(result: &Value) -> &str {
    result["content"][0]["text"]
        .as_str()
        .expect("a tool result has explanatory text")
}

fn success(session: &mut McpSession, name: &str, arguments: Value) -> String {
    let result = tool(session, name, arguments);
    assert_ne!(result["isError"], true, "{result}");
    text(&result).to_owned()
}

fn failure(session: &mut McpSession, name: &str, arguments: Value, code: &str) {
    let result = tool(session, name, arguments);
    assert_eq!(result["isError"], true, "{result}");
    assert!(text(&result).starts_with(&format!("{code}: ")), "{result}");
}

fn scene(volume: u8) -> Value {
    json!({
        "name": SCENE,
        "scene": {
            "groups": [{ "coordinator": "Kitchen", "source": { "kind": "keep" } }],
            "volumes": { "Kitchen": volume }
        }
    })
}

fn add_schedule() -> Value {
    json!({ "when": "in 5m", "action": "volume", "zone": "Kitchen", "volume": 80 })
}

fn daemon_policy(session: &mut McpSession) {
    let policy = success(session, "get_policy", json!({}));
    assert!(policy.contains("Defaults: rooms up to 31,"), "{policy}");
    assert!(
        policy.contains("You (mcp-stdio): the volume caps apply."),
        "{policy}"
    );
}

fn configuration_reads(session: &mut McpSession, id: i64) {
    let scenes = success(session, "list_scenes", json!({}));
    assert!(
        scenes.starts_with(&format!("{SCENE}: Kitchen:")),
        "{scenes}"
    );
    assert!(!scenes.contains(LOCAL_SCENE), "{scenes}");
    let saved = success(session, "get_scene", json!({ "name": SCENE }));
    assert_eq!(saved, scenes);
    let schedules = success(session, "list_schedules", json!({}));
    assert!(
        schedules.contains(&format!("#{id} "))
            && schedules.contains("set Kitchen's volume to 80")
            && !schedules.contains("set Kitchen's volume to 12"),
        "{schedules}"
    );
}

fn scheduled_actions(fixture: &Fixture) -> Vec<Value> {
    fixture
        .get("/actions?client=mcp-stdio&limit=50")
        .as_array()
        .unwrap()
        .iter()
        .filter(|action| {
            action["intent"]
                .as_str()
                .is_some_and(|intent| intent.starts_with("set_volume"))
        })
        .cloned()
        .collect()
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the daemon finishes its scheduled action after stdio exits"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn stdio_configuration_stays_on_the_daemon_and_scheduled_work_outlives_the_agent() {
    let mut fixture = Fixture::new("mcp-daemon-config", DAEMON_POLICY);
    let mut session = fixture.session();
    daemon_policy(&mut session);
    success(&mut session, "save_scene", scene(28));
    assert_eq!(fixture.get(SCENE_PATH)["volumes"]["Kitchen"], 28);
    success(&mut session, "save_scene", scene(24));
    assert_eq!(fixture.get(SCENE_PATH)["volumes"]["Kitchen"], 24);
    success(&mut session, "add_schedule", add_schedule());
    let schedules = fixture.get("/schedules");
    assert_eq!(schedules.as_array().unwrap().len(), 1, "{schedules}");
    let id = schedules[0]["id"].as_i64().unwrap();
    assert_eq!(schedules[0]["creator"], "mcp-stdio", "{schedules}");
    assert_eq!(schedules[0]["volume"], 80, "{schedules}");
    assert_eq!(schedules[0]["enabled"], true, "{schedules}");
    configuration_reads(&mut session, id);
    success(&mut session, "pause_schedule", json!({ "id": id }));
    assert_eq!(fixture.schedule(id)["enabled"], false);
    success(&mut session, "resume_schedule", json!({ "id": id }));
    assert_eq!(fixture.schedule(id)["enabled"], true);
    drop(session);

    // A fresh stdio process reads the same authoritative configuration.
    fixture.local_unchanged();
    let mut session = fixture.session();
    configuration_reads(&mut session, id);
    success(&mut session, "apply_scene", json!({ "name": SCENE }));
    assert_eq!(fixture.volume(), 24);
    drop(session);

    // No MCP process remains. Only serve can run the stored schedule, using
    // its creator's current policy rather than the local CLI's exemption.
    std::fs::write(&fixture.clock, DUE).unwrap();
    wait_for(|| fixture.volume() == 31 && scheduled_actions(&fixture).len() == 1);
    let actions = scheduled_actions(&fixture);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(actions[0]["client"], "mcp-stdio", "{actions:?}");
    assert_eq!(actions[0]["surface"], "serve", "{actions:?}");
    assert!(
        actions[0]["decision"]
            .as_str()
            .is_some_and(|decision| decision.starts_with("clamp: ")),
        "{actions:?}"
    );
    assert!(fixture.schedule(id)["next"].is_null());

    let mut session = fixture.session();
    success(&mut session, "remove_schedule", json!({ "id": id }));
    assert!(fixture.get("/schedules").as_array().unwrap().is_empty());
    success(&mut session, "delete_scene", json!({ "name": SCENE }));
    assert!(fixture.get("/scenes").as_array().unwrap().is_empty());
    drop(session);
    fixture.local_unchanged();
    fixture.finish();
}

fn remote_misses(session: &mut McpSession, local_id: i64) {
    failure(
        session,
        "get_scene",
        json!({ "name": LOCAL_SCENE }),
        "UNKNOWN_SCENE",
    );
    failure(
        session,
        "delete_scene",
        json!({ "name": LOCAL_SCENE }),
        "UNKNOWN_SCENE",
    );
    for name in ["pause_schedule", "resume_schedule", "remove_schedule"] {
        failure(session, name, json!({ "id": local_id }), "UNKNOWN_SCHEDULE");
    }
}

#[test]
fn daemon_errors_and_loss_never_fall_back_to_the_agents_local_configuration() {
    let mut fixture = Fixture::new("mcp-daemon-config-refusals", DENIED_POLICY);
    let mut session = fixture.session();
    daemon_policy(&mut session);
    assert!(success(&mut session, "list_scenes", json!({})).starts_with("No scenes saved yet"));
    assert_eq!(
        success(&mut session, "list_schedules", json!({})),
        "no schedules"
    );
    let local_id = fixture.local_schedules[0]["id"].as_i64().unwrap();
    remote_misses(&mut session, local_id);
    failure(&mut session, "save_scene", scene(80), "POLICY_DENIED");
    failure(
        &mut session,
        "add_schedule",
        add_schedule(),
        "POLICY_DENIED",
    );
    assert!(fixture.get("/scenes").as_array().unwrap().is_empty());
    assert!(fixture.get("/schedules").as_array().unwrap().is_empty());
    assert_eq!(fixture.volume(), 20);

    // Keep the same initialized stdio connection after serve disappears.
    // The copied connection record is stale, and --daemon remains required.
    fixture.stop();
    let before = fixture.scenario.sim_handle().unwrap().soap_log().len();
    for (name, arguments) in [
        ("list_scenes", json!({})),
        ("get_scene", json!({ "name": LOCAL_SCENE })),
        ("get_policy", json!({})),
        ("list_schedules", json!({})),
        ("save_scene", scene(80)),
        ("add_schedule", add_schedule()),
        ("delete_scene", json!({ "name": LOCAL_SCENE })),
        ("remove_schedule", json!({ "id": local_id })),
    ] {
        failure(&mut session, name, arguments, "NOT_READY");
    }
    drop(session);
    assert_eq!(
        fixture.scenario.sim_handle().unwrap().soap_log().len(),
        before,
        "failed daemon selection causes no local speaker reads or writes"
    );
    fixture.local_unchanged();
    fixture.finish();
}
