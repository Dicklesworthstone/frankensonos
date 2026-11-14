use super::*;
use fsonos_types::HouseholdId;

fn player(id: &str, address: u8, attributes: &str) -> String {
    format!(
        "<ZoneGroupMember UUID=\"{id}\" Location=\"http://192.0.2.{address}:1400/xml/device_description.xml\" \
         ZoneName=\"{id}\" {attributes}/>"
    )
}

fn group(coordinator: &str, members: &str) -> String {
    format!("<ZoneGroup Coordinator=\"{coordinator}\">{members}</ZoneGroup>")
}

fn document(groups: &str) -> String {
    format!("<ZoneGroupState><ZoneGroups>{groups}</ZoneGroups></ZoneGroupState>")
}

fn houses() -> Vec<HouseholdState> {
    [
        group("a", &(player("a", 1, "") + &player("b", 2, ""))),
        group("other", &player("other", 10, "")),
    ]
    .iter()
    .enumerate()
    .map(|(index, groups)| {
        let mut house = HouseholdState {
            id: Some(HouseholdId(format!("synthetic-house-{index}"))),
            ..HouseholdState::default()
        };
        house.apply_topology(&parse_zone_group_state(&document(groups)).unwrap());
        house
    })
    .collect()
}

fn rejects_without_replacing(document: &str) {
    // These are valid XML and protocol shapes; parsing alone is not
    // evidence that they may replace the currently controlled house.
    parse_zone_group_state(document).unwrap();
    let mut houses = houses();
    let before = houses.clone();
    assert!(apply(&mut houses, &PlayerId("a".into()), document).is_err());
    assert_eq!(houses, before, "both households and rooms stay intact");
}

#[test]
fn empty_partial_and_incoherent_groups_cannot_erase_the_sender_household() {
    let a = player("a", 1, "");
    let b = player("b", 2, "");
    for groups in [
        String::new(),
        group("b", &b),
        group("missing", &a),
        group("a", ""),
        group("a", &a) + &group("a", &a),
        group("a", &a) + &group("b", &(b.clone() + &a)),
        group("a", &(a.clone() + &player("", 3, ""))),
        group("a", &(a.clone() + &player("b", 1, ""))),
        group("a", &a.replace("http://192.0.2.1:1400/xml/device_description.xml", "unknown")),
    ] {
        rejects_without_replacing(&document(&groups));
    }
}

#[test]
fn satellites_and_vanished_reports_do_not_bypass_identity_checks() {
    let a = player("a", 1, "");
    let repeated = a.replace(
        "/>",
        "><Satellite UUID=\"a\" Location=\"http://192.0.2.2:1400/x\"/></ZoneGroupMember>",
    );
    rejects_without_replacing(&document(&group("a", &repeated)));
    for vanished in [
        "<Device UUID=\"a\"/>",
        "<Device UUID=\"other\"/>",
        "<Device UUID=\"b\"/><Device UUID=\"b\"/>",
    ] {
        let doc = document(&group("a", &a)).replace(
            "</ZoneGroupState>",
            &format!("<VanishedDevices>{vanished}</VanishedDevices></ZoneGroupState>"),
        );
        rejects_without_replacing(&doc);
    }
}

#[test]
fn known_other_households_cannot_be_imported_by_id_or_control_address() {
    for foreign in [player("other", 11, ""), player("new", 10, "")] {
        rejects_without_replacing(&document(&group(
            "a",
            &(player("a", 1, "") + &foreign),
        )));
    }
}

#[test]
fn unknown_or_ambiguous_senders_cannot_reassign_a_household() {
    let doc = document(&group("a", &player("a", 1, "")));
    for sender in ["unseen", "a"] {
        let mut houses = houses();
        if sender == "a" {
            houses.push(houses[0].clone());
        }
        let before = houses.clone();
        assert!(apply(&mut houses, &PlayerId(sender.into()), &doc).is_err());
        assert_eq!(houses, before);
    }
}

#[test]
fn valid_regrouping_new_bonds_and_peer_disappearance_are_adopted_together() {
    let mut houses = houses();
    let other = houses[1].clone();
    houses[0].players[0].model = "learned model".into();
    let paired = "<ZoneGroupMember UUID=\"a\" Location=\"http://192.0.2.1:1400/x\" \
        ZoneName=\"New name\" BootSeq=\"9\" HTSatChanMapSet=\"a:LF,RF;sub:SW\">\
        <Satellite UUID=\"sub\" Location=\"http://192.0.2.3:1400/x\" \
        ZoneName=\"New name\" Invisible=\"1\"/></ZoneGroupMember>";
    let doc = document(&group("new", &(player("new", 4, "") + paired))).replace(
        "</ZoneGroupState>",
        "<VanishedDevices><Device UUID=\"b\"/></VanishedDevices></ZoneGroupState>",
    );
    apply(&mut houses, &PlayerId("a".into()), &doc).unwrap();
    assert_eq!(houses[0].id, Some(HouseholdId("synthetic-house-0".into())));
    assert_eq!(houses[0].players.len(), 3);
    assert_eq!(houses[0].rooms.len(), 2);
    assert!(houses[0].player(&PlayerId("b".into())).is_none());
    let a = houses[0].player(&PlayerId("a".into())).unwrap();
    assert_eq!(a.model, "learned model");
    assert_eq!(a.room_name, "New name");
    assert_eq!(houses[0].groups[0].coordinator, PlayerId("new".into()));
    assert_eq!(houses[0].rooms[1].players, [PlayerId("a".into()), PlayerId("sub".into())]);
    assert_eq!(houses[1], other);
}

#[test]
fn an_invisible_surviving_pair_member_can_still_report_its_household() {
    let mut houses = houses();
    let doc = document(&group(
        "a",
        &player("a", 1, "Invisible=\"1\" ChannelMapSet=\"a:RF;missing:LF\""),
    ));
    apply(&mut houses, &PlayerId("a".into()), &doc).unwrap();
    assert_eq!(houses[0].rooms[0].primary, PlayerId("a".into()));
    assert_eq!(houses[0].rooms[0].missing, [PlayerId("missing".into())]);
}

#[test]
fn changed_addresses_and_ordinary_bridges_are_admitted() {
    let mut houses = houses();
    let bridge = group(
        "bridge",
        &player("bridge", 20, "IsZoneBridge=\"1\" Invisible=\"1\""),
    );
    let doc = document(
        &(group("a", &(player("a", 9, "BootSeq=\"4\"") + &player("b", 2, "")))
            + &bridge),
    );
    apply(&mut houses, &PlayerId("a".into()), &doc).unwrap();
    assert_eq!(
        houses[0].player(&PlayerId("a".into())).unwrap().ip.to_string(),
        "192.0.2.9"
    );
    assert_eq!(houses[0].players.len(), 2);
    assert_eq!(houses[0].groups.len(), 1);
    assert!(houses[0].player(&PlayerId("bridge".into())).is_none());
}

#[test]
fn a_known_satellite_sender_can_report_a_coherent_household() {
    let mut houses = houses();
    let paired = "<ZoneGroupMember UUID=\"a\" Location=\"http://192.0.2.1:1400/x\" \
        ZoneName=\"TV\" HTSatChanMapSet=\"a:LF,RF;sub:SW\">\
        <Satellite UUID=\"sub\" Location=\"http://192.0.2.3:1400/x\" \
        ZoneName=\"TV\" Invisible=\"1\"/></ZoneGroupMember>";
    let doc = document(&group("a", paired));
    houses[0].apply_topology(&parse_zone_group_state(&doc).unwrap());
    apply(&mut houses, &PlayerId("sub".into()), &doc).unwrap();
    assert_eq!(houses[0].players.len(), 2);
    assert_eq!(houses[0].rooms.len(), 1);
    assert_eq!(houses[0].rooms[0].players, [PlayerId("a".into()), PlayerId("sub".into())]);
}
