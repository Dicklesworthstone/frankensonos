use super::*;
use crate::failure::ErrorCode;
use crate::zones::fixtures::households;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Policy;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn names(view: &ResolvedRoomsDto) -> Vec<String> {
    view.rooms
        .iter()
        .map(|room| format!("{}@{}", room.room, room.household))
        .collect()
}

#[test]
fn an_alias_expands_ambiguous_entries_exactly_as_the_command_does() {
    let aliases = Aliases::parse("[aliases]\ntogether = ['Kitchen', 'Patio@S2']").unwrap();
    let view = resolved(&households(), Some(&aliases), &Client::Unknown, "together").unwrap();
    assert_eq!(view.matched, "alias");
    assert_eq!(names(&view), ["Kitchen@S1", "Kitchen@S2", "Patio@S2"]);
    assert_eq!(view.groups.len(), 3);
    let group = view.groups.iter().find(|g| g.household == "S1").unwrap();
    assert_eq!(group.coordinator_room, "Den");
    assert_eq!(
        group
            .members
            .iter()
            .map(|r| r.room.as_str())
            .collect::<Vec<_>>(),
        ["Den", "Kitchen"]
    );
}

#[test]
fn one_member_names_one_room_but_its_complete_transport_group() {
    let view = resolved(&households(), None, &Client::Unknown, "Kitchen@S1").unwrap();
    assert_eq!(view.matched, "room");
    assert_eq!(names(&view), ["Kitchen@S1"]);
    assert_eq!(view.rooms[0].player, "RINCON_KIT1");
    assert_eq!(view.rooms[0].coordinator, "RINCON_DEN");
    assert_eq!(view.groups.len(), 1);
    assert_eq!(view.groups[0].members.len(), 2);
}

#[test]
fn everywhere_and_household_qualification_preserve_bonded_room_identity() {
    let all = resolved(&households(), None, &Client::Unknown, "everywhere").unwrap();
    assert_eq!(all.rooms.len(), 5);
    assert_eq!(all.groups.len(), 4);
    let s1 = resolved(&households(), None, &Client::Unknown, "all@S1").unwrap();
    assert_eq!(s1.rooms.len(), 3);
    assert_eq!(s1.groups.len(), 2);
    assert!(s1.rooms.iter().all(|room| room.household == "S1"));
    assert_eq!(
        s1.rooms
            .iter()
            .filter(|room| room.room == "Ada’s Studio")
            .count(),
        1
    );
}

#[test]
fn direct_room_precedence_and_client_specific_here_are_not_browser_guesses() {
    let aliases = Aliases::parse(
        "[aliases]\nDen = ['Kitchen@S2']\n[defaults]\nroom = 'Den'\n[defaults.clients.'listener@example.test']\nroom = 'Patio@S2'",
    )
    .unwrap();
    let direct = resolved(&households(), Some(&aliases), &Client::Unknown, "Den").unwrap();
    assert_eq!(direct.matched, "room");
    assert_eq!(names(&direct), ["Den@S1"]);
    let here = resolved(
        &households(),
        Some(&aliases),
        &Client::Tailnet("listener@example.test".into()),
        "here",
    )
    .unwrap();
    assert_eq!(here.matched, "here");
    assert_eq!(names(&here), ["Patio@S2"]);
}

#[test]
fn ambiguous_unknown_and_prefix_queries_keep_core_semantics() {
    let h = households();
    assert_eq!(
        resolved(&h, None, &Client::Unknown, "Kitchen")
            .unwrap_err()
            .code,
        ErrorCode::AmbiguousRoom
    );
    assert_eq!(
        resolved(&h, None, &Client::Unknown, "not-a-room")
            .unwrap_err()
            .code,
        ErrorCode::UnknownRoom
    );
    let prefix = resolved(&h, None, &Client::Unknown, "Pat").unwrap();
    assert_eq!(prefix.matched, "prefix");
    assert_eq!(names(&prefix), ["Patio@S2"]);
}

#[test]
fn repeated_alias_rooms_and_inventory_order_do_not_duplicate_or_reorder_the_view() {
    let aliases = Aliases::parse("[aliases]\nrooms = ['Den', 'Kitchen@S1', 'Den']").unwrap();
    let mut h = households();
    let first = resolved(&h, Some(&aliases), &Client::Unknown, "rooms").unwrap();
    assert_eq!(first.rooms.len(), 2);
    assert_eq!(first.groups.len(), 1);
    h.reverse();
    for house in &mut h {
        house.rooms.reverse();
    }
    assert_eq!(
        resolved(&h, Some(&aliases), &Client::Unknown, "rooms").unwrap(),
        first
    );
    let json = serde_json::to_string(&first).unwrap();
    assert!(
        !json.contains("192.0.2.10"),
        "a target preview needs identities, not addresses"
    );
}

struct NoSpeakerIo(Arc<AtomicUsize>);
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ProtoError::NotWired(
            "no speaker reads in target resolution",
        ))
    }
}

fn surface(policy: Policy) -> (Surface, Arc<AtomicUsize>, Arc<AtomicUsize>) {
    let io = Arc::new(AtomicUsize::new(0));
    let surveys = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&surveys);
    let surface = Surface::new(
        Box::new(NoSpeakerIo(Arc::clone(&io))),
        Box::new(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Ok(households())
        }),
        policy,
        Box::new(SystemClock),
    );
    (surface, surveys, io)
}

#[test]
fn both_read_permissions_and_target_validation_precede_discovery() {
    for denied in ["resolve_rooms", "list_rooms", "list_zones"] {
        let policy =
            Policy::from_toml(&format!("[clients.'loopback-http']\ndeny = ['{denied}']")).unwrap();
        let (surface, surveys, io) = surface(policy);
        assert_eq!(
            surface
                .resolve_rooms(&Client::LoopbackHttp, "everywhere")
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied
        );
        assert_eq!(surveys.load(Ordering::SeqCst), 0);
        assert_eq!(io.load(Ordering::SeqCst), 0);
    }
    let (surface, surveys, io) = surface(Policy::default());
    for query in [" ".to_string(), "x".repeat(129)] {
        assert_eq!(
            surface
                .resolve_rooms(&Client::Unknown, &query)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(surveys.load(Ordering::SeqCst), 0);
    assert_eq!(
        surface
            .resolve_rooms(&Client::Unknown, " everywhere ")
            .unwrap()
            .target,
        "everywhere"
    );
    assert_eq!(surveys.load(Ordering::SeqCst), 1);
    assert_eq!(io.load(Ordering::SeqCst), 0);
}

#[test]
fn the_surface_rereads_alias_edits_instead_of_reusing_a_previous_resolution() {
    let (surface, _, io) = surface(Policy::default());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    std::fs::write(&path, "[aliases]\nlistening = ['Den']").unwrap();
    let surface = surface.with_aliases_file(path.clone());
    let before = surface
        .resolve_rooms(&Client::Unknown, "listening")
        .unwrap();
    std::fs::write(&path, "[aliases]\nlistening = ['Kitchen']").unwrap();
    let after = surface
        .resolve_rooms(&Client::Unknown, "listening")
        .unwrap();
    assert_eq!(names(&before), ["Den@S1"]);
    assert_eq!(names(&after), ["Kitchen@S1", "Kitchen@S2"]);
    assert_ne!(before, after);
    assert_eq!(io.load(Ordering::SeqCst), 0);
}
