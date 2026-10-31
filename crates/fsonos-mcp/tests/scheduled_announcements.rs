//! The registered tool keeps the nested scheduled recipe available to agents.

use fsonos_mcp::{Profile, server_with};

#[test]
fn add_schedule_advertises_the_optional_announcement_recipe() {
    let server = server_with(Vec::new(), Profile::Full);
    let tools = server.tools();
    let tool = tools
        .iter()
        .find(|tool| tool.name == "add_schedule")
        .unwrap();
    let value = serde_json::to_value(tool).unwrap();
    let schema = &value["inputSchema"];
    assert!(
        schema["properties"].get("announcement").is_some(),
        "{schema}"
    );
    assert!(
        !schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "announcement")
    );
    let description = value["description"].as_str().unwrap();
    for detail in [
        "'announce'",
        "announcement:",
        "text",
        "chime",
        "rooms",
        "voice",
        "volume",
        "Uploaded WAVs cannot be scheduled",
    ] {
        assert!(description.contains(detail), "{detail}: {description}");
    }
}
