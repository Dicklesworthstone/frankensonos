//! The selected daemon also owns saved house state and the scheduler.

use super::*;

const ROOM: &str = "Room / &@S2";
const NAME: &str = "Evening / &";

struct Case {
    tool: &'static str,
    method: &'static str,
    path: &'static str,
    body: Option<Value>,
    answer: Value,
    envelope: Option<&'static str>,
}

fn scene() -> Value {
    json!({
        "name": NAME, "saved": 42,
        "groups": [{"coordinator": ROOM, "members": [],
            "source": {"kind": "keep"}, "playing": true}],
        "volumes": {(ROOM): 31}, "mutes": {}
    })
}

fn definition() -> Value {
    json!({
        "groups": [{"coordinator": ROOM, "source": {"kind": "keep"}}],
        "volumes": {(ROOM): 31}, "mutes": {(ROOM): false}
    })
}

fn schedule(enabled: bool) -> Value {
    json!({
        "id": 17, "when": "weekdays 07:30", "action": "apply_scene",
        "scene": NAME, "summary": "apply the evening scene",
        "creator": "mcp-stdio", "enabled": enabled
    })
}

fn tone() -> Value {
    json!({"room": ROOM, "bass": 3, "treble": -1, "loudness": false})
}

fn reads() -> Vec<Case> {
    vec![
        Case {
            tool: "list_players",
            method: "GET",
            path: "/players",
            body: None,
            answer: json!([{
                "room": ROOM, "id": "RINCON_REMOTE", "model": "Era 100",
                "generation": "S2", "ip": "192.0.2.7", "household": "S2",
                "coordinator_room": ROOM
            }]),
            envelope: Some("players"),
        },
        Case {
            tool: "get_policy",
            method: "GET",
            path: "/policy",
            body: None,
            answer: json!({
                "you": "mcp-stdio", "you_are_capped": true, "quiet_hours_now": false,
                "defaults": {"max_volume": 31, "max_step": 12, "fade_secs": 0},
                "rooms": [], "clients": []
            }),
            envelope: None,
        },
        Case {
            tool: "get_eq",
            method: "GET",
            path: "/zones/Room%20%2F%20%26%40S2/eq",
            body: None,
            answer: tone(),
            envelope: None,
        },
        Case {
            tool: "list_scenes",
            method: "GET",
            path: "/scenes",
            body: None,
            answer: json!([scene()]),
            envelope: Some("scenes"),
        },
        Case {
            tool: "get_scene",
            method: "GET",
            path: "/scenes/Evening%20%2F%20%26",
            body: None,
            answer: scene(),
            envelope: None,
        },
        Case {
            tool: "list_schedules",
            method: "GET",
            path: "/schedules",
            body: None,
            answer: json!([schedule(true)]),
            envelope: Some("schedules"),
        },
    ]
}

fn writes() -> Vec<Case> {
    vec![
        Case {
            tool: "set_eq",
            method: "POST",
            path: "/eq",
            body: Some(json!({"zone": ROOM, "bass": 3, "loudness": false})),
            answer: json!({"done": "the daemon changed the tone", "eq": tone()}),
            envelope: None,
        },
        Case {
            tool: "save_scene",
            method: "PUT",
            path: "/scenes/Evening%20%2F%20%26",
            body: Some(definition()),
            answer: scene(),
            envelope: None,
        },
        Case {
            tool: "delete_scene",
            method: "DELETE",
            path: "/scenes/Evening%20%2F%20%26",
            body: None,
            answer: json!({"deleted": NAME, "done": "the scene is gone"}),
            envelope: None,
        },
        Case {
            tool: "add_schedule",
            method: "POST",
            path: "/schedules",
            body: Some(json!({
                "when": "weekdays 07:30", "action": "apply_scene", "scene": NAME,
                "zone": null, "mood": null, "volume": null, "fade_secs": null
            })),
            answer: schedule(true),
            envelope: None,
        },
        Case {
            tool: "pause_schedule",
            method: "POST",
            path: "/schedules/pause",
            body: Some(json!({"id": 17})),
            answer: schedule(false),
            envelope: None,
        },
        Case {
            tool: "resume_schedule",
            method: "POST",
            path: "/schedules/resume",
            body: Some(json!({"id": 17})),
            answer: schedule(true),
            envelope: None,
        },
        Case {
            tool: "remove_schedule",
            method: "POST",
            path: "/schedules/remove",
            body: Some(json!({"id": 17})),
            answer: schedule(true),
            envelope: None,
        },
    ]
}

fn invoke(backend: &Backend, case: &Case) -> McpResult<FinalCallToolResult> {
    match case.tool {
        "list_players" => backend.list_players(),
        "get_policy" => backend.get_policy(),
        "get_eq" => backend.get_eq(ROOM),
        "set_eq" => {
            backend.set_eq(&serde_json::from_value(case.body.clone().unwrap()).unwrap())
        }
        "list_scenes" => backend.list_scenes(),
        "get_scene" => backend.get_scene(NAME),
        "save_scene" => backend.save_scene_from(NAME, case.body.as_ref().unwrap()),
        "delete_scene" => backend.delete_scene(NAME),
        "list_schedules" => backend.list_schedules(),
        "add_schedule" => {
            backend.add_schedule(&serde_json::from_value(case.body.clone().unwrap()).unwrap())
        }
        "pause_schedule" => backend.pause_schedule(17, true),
        "resume_schedule" => backend.pause_schedule(17, false),
        "remove_schedule" => backend.remove_schedule(17),
        _ => panic!("unlisted daemon operation"),
    }
}

#[test]
fn saved_state_inventory_and_policy_reads_use_the_selected_daemon() {
    for case in reads() {
        let (backend, reply) = fixture(Some(Ok(case.answer.clone())));
        let result = invoke(&backend, &case).unwrap();
        let expected = case.envelope.map_or_else(
            || case.answer.clone(),
            |key| json!({(key): case.answer}),
        );
        assert_eq!(result.structured_content, Some(expected), "{}", case.tool);
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!([case.method, case.path, case.body])],
            "{}",
            case.tool,
        );
    }
}

#[test]
fn house_edits_use_the_daemon_store_scheduler_and_action_log_once() {
    for case in writes() {
        let (backend, reply) = fixture(Some(Ok(case.answer.clone())));
        let result = invoke(&backend, &case).unwrap();
        assert_eq!(result.structured_content, Some(case.answer), "{}", case.tool);
        assert_eq!(
            *reply.calls.lock().unwrap(),
            [json!([case.method, case.path, case.body])],
            "{}", case.tool,
        );
        assert!(!text(&result).is_empty());
    }
}

#[test]
fn denied_lost_and_malformed_daemon_replies_never_try_local_state() {
    for case in reads().into_iter().chain(writes()) {
        for answer in [
            Err(Failure::new(ErrorCode::PolicyDenied, "daemon policy denied")),
            Err(Failure::new(ErrorCode::PlayerUnreachable, "reply lost")),
            Ok(json!({"not_the_expected_document": true})),
        ] {
            let (backend, reply) = fixture(Some(answer));
            assert!(invoke(&backend, &case).is_err(), "{}", case.tool);
            assert_eq!(reply.calls.lock().unwrap().len(), 1, "{}", case.tool);
        }
    }
}

#[test]
fn no_selected_daemon_keeps_standalone_house_edits_subject_to_local_policy() {
    for case in writes() {
        let (backend, reply) = fixture(None);
        let error = invoke(&backend, &case).unwrap_err();
        assert!(
            error.message.contains("POLICY_DENIED"),
            "{}: {error:?}",
            case.tool
        );
        assert_eq!(reply.calls.lock().unwrap().len(), 1, "{}", case.tool);
    }
}

#[test]
fn a_malformed_explicit_scene_is_rejected_before_any_daemon_save() {
    let (backend, reply) = fixture(Some(Ok(scene())));
    let error = backend
        .save_scene_from(NAME, &json!({"unknown_field": true}))
        .unwrap_err();
    assert!(error.message.contains("INVALID_ARGUMENT"), "{error:?}");
    assert!(reply.calls.lock().unwrap().is_empty());
}
