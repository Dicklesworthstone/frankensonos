use super::*;
use fsonos_proto::topology::parse_zone_group_state;

fn host() -> IpAddr {
    "192.0.2.10".parse().unwrap()
}

fn coordinator() -> PlayerId {
    PlayerId("RINCON_COORDINATOR".into())
}

fn topology(boot: &str, generation: &str) -> ZoneGroupState {
    parse_zone_group_state(&format!(
        r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_COORDINATOR" ID="group:1">
        <ZoneGroupMember UUID="RINCON_COORDINATOR" ZoneName="Test room"
        Location="http://192.0.2.10:1400/xml/device_description.xml" {boot} {generation}/>
        </ZoneGroup></ZoneGroups>"#
    ))
    .unwrap()
}

fn owner(topology: &ZoneGroupState) -> Owner {
    Owner {
        host: host(),
        group: group(topology, host(), &coordinator()).unwrap(),
        source: "x-rincon-queue:RINCON_COORDINATOR#0".into(),
    }
}

#[test]
fn reuse_requires_an_observed_boot_and_a_supported_generation() {
    for generation in [1, 2] {
        assert!(
            owner(&topology(
                r#"BootSeq="7""#,
                &format!(r#"SWGen="{generation}""#)
            ))
            .reusable()
        );
    }
    for (boot, generation) in [
        ("", r#"SWGen="2""#),
        (r#"BootSeq="7""#, ""),
        (r#"BootSeq="7""#, r#"SWGen="3""#),
    ] {
        assert!(!owner(&topology(boot, generation)).reusable());
    }
}

#[test]
fn reboot_generation_and_source_changes_invalidate_the_original_owner() {
    let before = owner(&topology(r#"BootSeq="7""#, r#"SWGen="2""#));
    assert_ne!(before, owner(&topology(r#"BootSeq="8""#, r#"SWGen="2""#)));
    assert_ne!(before, owner(&topology(r#"BootSeq="7""#, r#"SWGen="1""#)));
    let mut changed = owner(&topology(r#"BootSeq="7""#, r#"SWGen="2""#));
    changed.source = "x-sonosapi-stream:external".into();
    assert_ne!(before, changed);
}

#[test]
fn ambiguous_missing_bridge_and_wrong_address_coordinators_are_rejected() {
    let original = topology(r#"BootSeq="7""#, r#"SWGen="2""#);
    let mut duplicate = original.clone();
    duplicate.groups.push(duplicate.groups[0].clone());
    assert!(group(&duplicate, host(), &coordinator()).is_err());
    let mut missing = original.clone();
    missing.groups[0].members.clear();
    assert!(group(&missing, host(), &coordinator()).is_err());
    let mut bridge = original.clone();
    bridge.groups[0].members[0].is_zone_bridge = true;
    assert!(group(&bridge, host(), &coordinator()).is_err());
    let mut wrong_address = original;
    wrong_address.groups[0].members[0].location = "http://192.0.2.11:1400/".into();
    assert!(group(&wrong_address, host(), &coordinator()).is_err());
}

#[test]
fn bonded_membership_is_canonical_but_real_group_changes_invalidate() {
    let original = topology(r#"BootSeq="7""#, r#"SWGen="2""#);
    let mut reordered = original.clone();
    let duplicate = reordered.groups[0].members[0].clone();
    // A repeated bond identity is not a new room.
    reordered.groups[0].members[0].satellites.push(duplicate);
    assert_eq!(owner(&original), owner(&reordered));
    reordered.groups[0].members[0].satellites[0].uuid = PlayerId("RINCON_SATELLITE".into());
    assert_ne!(owner(&original), owner(&reordered));
    let mut changed_group = original.clone();
    changed_group.groups[0].id = "group:2".into();
    assert_ne!(owner(&original), owner(&changed_group));
}
