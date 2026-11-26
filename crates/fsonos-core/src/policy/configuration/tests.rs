use super::*;
use std::sync::{Arc, Barrier};

fn defaults(max_volume: u8) -> PolicyEdit {
    PolicyEdit::SetDefaults {
        max_volume,
        max_step: 12,
        fade_secs: 3,
    }
}

fn client(name: &str) -> PolicyEdit {
    PolicyEdit::SetClient {
        client: name.into(),
        allow: Some(vec!["read".into(), "playback".into()]),
        deny: vec!["next".into()],
        grant: vec!["auth".into()],
        capped: Some(true),
        manage_others: false,
        mcp_profile: Some(McpProfile::Essentials),
    }
}

fn owner(policy: &Policy) -> Result<(), String> {
    authorize(policy, &Client::Cli)
}

#[test]
fn missing_policy_no_ops_do_not_create_it_and_empty_has_a_distinct_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let missing = read(&path).unwrap();
    assert_eq!(missing.policy, Policy::default());
    for change in [
        PolicyEdit::ClearQuietHours,
        PolicyEdit::RemoveRoom {
            room: "Offline".into(),
        },
        PolicyEdit::RemoveClient {
            client: "missing".into(),
        },
        PolicyEdit::SetDefaults {
            max_volume: 70,
            max_step: 20,
            fade_secs: 0,
        },
    ] {
        let saved = apply(&path, &missing.revision, &change, owner).unwrap();
        assert!(!saved.changed);
        assert_eq!(saved.snapshot, missing);
        assert!(!path.exists());
    }
    fs::write(&path, "").unwrap();
    let empty = read(&path).unwrap();
    assert_eq!(empty.policy, missing.policy);
    assert_ne!(empty.revision, missing.revision);
    assert!(matches!(
        apply(&path, &missing.revision, &defaults(60), owner),
        Err(ConfigurationError::Conflict { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), b"");
}

#[test]
fn complete_rules_round_trip_with_inheritance_and_empty_allow_list_distinguished() {
    let mut text = edit("", &defaults(65)).unwrap();
    text = edit(
        &text,
        &PolicyEdit::SetQuietHours {
            start: "22:00".into(),
            end: "07:00".into(),
            max_volume: 20,
        },
    )
    .unwrap();
    text = edit(
        &text,
        &PolicyEdit::SetRoom {
            room: "Offline Bedroom".into(),
            max_volume: Some(35),
            max_step: None,
        },
    )
    .unwrap();
    text = edit(&text, &client("Tag:Tablet")).unwrap();
    let shown = Policy::from_toml(&text).unwrap().view();
    assert_eq!(shown.defaults.max_volume, 65);
    assert_eq!(shown.defaults.fade_secs, 3);
    assert_eq!(shown.quiet_hours.unwrap().end, "07:00");
    assert_eq!(shown.rooms[0].max_step, None);
    assert_eq!(shown.clients[0].client, "tag:tablet");
    assert_eq!(shown.clients[0].mcp_profile, Some(McpProfile::Essentials));
    let mut restricted = client("TAG:TABLET");
    if let PolicyEdit::SetClient {
        allow,
        deny,
        grant,
        capped,
        manage_others,
        mcp_profile,
        ..
    } = &mut restricted
    {
        *allow = Some(vec![]);
        deny.clear();
        grant.clear();
        *capped = None;
        *manage_others = true;
        *mcp_profile = Some(McpProfile::Full);
    }
    text = edit(&text, &restricted).unwrap();
    assert!(
        text.contains("[clients.\"Tag:Tablet\"]"),
        "original spelling retained: {text}"
    );
    let restricted_view = Policy::from_toml(&text).unwrap().view();
    assert_eq!(restricted_view.clients[0].allow, Some(vec![]));
    assert!(restricted_view.clients[0].deny.is_empty());
    assert_eq!(restricted_view.clients[0].capped, None);
    assert!(restricted_view.clients[0].manage_others);
    if let PolicyEdit::SetClient { allow, .. } = &mut restricted {
        *allow = None;
    }
    text = edit(&text, &restricted).unwrap();
    assert_eq!(
        Policy::from_toml(&text).unwrap().view().clients[0].allow,
        None
    );
    for change in [
        PolicyEdit::ClearQuietHours,
        PolicyEdit::RemoveRoom {
            room: "offline bedroom".into(),
        },
        PolicyEdit::RemoveClient {
            client: "tag:tablet".into(),
        },
    ] {
        text = edit(&text, &change).unwrap();
    }
    let cleared = Policy::from_toml(&text).unwrap().view();
    assert!(
        cleared.quiet_hours.is_none() && cleared.rooms.is_empty() && cleared.clients.is_empty()
    );
    assert_eq!(cleared.defaults.max_volume, 65);
}

#[test]
fn section_edits_preserve_comments_other_rules_and_crlf() {
    let before = "# owner note\r\n[defaults] # limits\r\nmax_volume = 70 # volume note\r\nmax_step = 20\r\n\r\n[rooms.\"Ada's room\"] # spelling\r\nmax_volume = 40 # quiet\r\nmax_step = 5 # override\r\n# retained independent note\r\n[clients.tablet]\r\ndeny = ['volume'] # untouched\r\n";
    let after = edit(before, &defaults(60)).unwrap();
    assert!(after.starts_with("# owner note\r\n[defaults] # limits\r\nfade_secs = 3\r\n"));
    assert!(after.contains("max_volume = 60 # volume note\r\n"));
    let room_start = before.find("[rooms.").unwrap();
    assert!(after.ends_with(&before[room_start..]));
    let changed = edit(
        &after,
        &PolicyEdit::SetRoom {
            room: "ADA’S ROOM".into(),
            max_volume: Some(30),
            max_step: None,
        },
    )
    .unwrap();
    assert!(changed.contains("[rooms.\"Ada's room\"] # spelling\r\nmax_volume = 30 # quiet\r\n"));
    assert!(!changed.contains("max_step = 5"));
    assert!(changed.ends_with(
        "# retained independent note\r\n[clients.tablet]\r\ndeny = ['volume'] # untouched\r\n"
    ));
    let removed = edit(
        &changed,
        &PolicyEdit::RemoveRoom {
            room: "Ada's room".into(),
        },
    )
    .unwrap();
    assert!(removed.contains("# retained independent note\r\n[clients.tablet]"));
    assert!(!removed.contains("Ada's room"));
}

#[test]
fn semantic_no_ops_retain_exact_omitted_defaults_comments_and_no_final_newline() {
    let before = "[defaults]\nmax_volume = 70 # no extra settings or newline";
    let same = PolicyEdit::SetDefaults {
        max_volume: 70,
        max_step: 20,
        fade_secs: 0,
    };
    assert_eq!(edit(before, &same).unwrap(), before);
    let before = "[clients.'Tag:Tablet']\nallow = [\n  'read', # commentary\n  'playback'\n]\ndeny = ['next']\ngrant = ['auth']\ncapped = true\nmcp_profile = 'essentials'";
    assert_eq!(edit(before, &client("TAG:TABLET")).unwrap(), before);
}

#[test]
fn inline_rules_can_be_replaced_without_touching_surrounding_entries() {
    let before = "# note\n[rooms]\nBedroom = { max_volume = 40, max_step = 5 } # target\nOffice = { max_volume = 65 } # keep\n";
    let after = edit(
        before,
        &PolicyEdit::SetRoom {
            room: "bedroom".into(),
            max_volume: Some(30),
            max_step: None,
        },
    )
    .unwrap();
    assert!(after.starts_with("# note\n[rooms]\nBedroom = "));
    assert!(after.ends_with(" # target\nOffice = { max_volume = 65 } # keep\n"));
    let removed = edit(
        &after,
        &PolicyEdit::RemoveRoom {
            room: "Bedroom".into(),
        },
    )
    .unwrap();
    assert_eq!(
        removed,
        "# note\n[rooms]\nOffice = { max_volume = 65 } # keep\n"
    );
}

#[test]
fn header_shaped_multiline_values_are_never_mistaken_for_target_sections() {
    let before = "[clients.fixture]\nallow = ['''\n[quiet_hours]\n'''] # old literal\n\n[quiet_hours] # real\nstart = '21:00'\nend = '08:00'\nmax_volume = 15\n";
    let after = edit(before, &PolicyEdit::ClearQuietHours).unwrap();
    assert_eq!(
        after,
        "[clients.fixture]\nallow = ['''\n[quiet_hours]\n'''] # old literal\n\n"
    );
}

#[test]
fn stale_revisions_never_overwrite_a_newer_editor_even_when_values_match() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let old = read(&path).unwrap();
    let saved = apply(&path, &old.revision, &defaults(60), owner).unwrap();
    let content = fs::read_to_string(&path).unwrap();
    fs::write(&path, format!("# authored since read\n{content}")).unwrap();
    let current = read(&path).unwrap();
    assert!(
        matches!(apply(&path, &saved.snapshot.revision, &defaults(60), owner), Err(ConfigurationError::Conflict { current_revision }) if current_revision == current.revision)
    );
    assert_eq!(read(&path).unwrap(), current);
}

#[test]
fn concurrent_editors_have_one_winner_and_never_lose_an_accepted_update() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let revision = read(&path).unwrap().revision;
    let barrier = Arc::new(Barrier::new(3));
    let handles: Vec<_> = [45, 55]
        .into_iter()
        .map(|volume| {
            let (path, revision, barrier) = (path.clone(), revision.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                apply(&path, &revision, &defaults(volume), owner)
            })
        })
        .collect();
    barrier.wait();
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ConfigurationError::Conflict { .. })))
            .count(),
        1
    );
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    assert_eq!(read(&path).unwrap(), winner.snapshot);
}

#[test]
fn proposed_rules_cannot_grant_the_caller_administration_or_undo_a_current_denial() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let agent = Client::McpStdio;
    let mut escalation = client("mcp-stdio");
    if let PolicyEdit::SetClient {
        allow,
        manage_others,
        ..
    } = &mut escalation
    {
        *allow = None;
        *manage_others = true;
    }
    let missing = read(&path).unwrap();
    assert!(matches!(
        apply(&path, &missing.revision, &escalation, |policy| authorize(
            policy, &agent
        )),
        Err(ConfigurationError::Denied(_))
    ));
    assert!(!path.exists());
    fs::write(
        &path,
        "[clients.mcp-stdio]\nmanage_others = true\ndeny = ['house-config']\n",
    )
    .unwrap();
    let observed = read(&path).unwrap();
    assert!(matches!(
        apply(&path, &observed.revision, &escalation, |policy| authorize(
            policy, &agent
        )),
        Err(ConfigurationError::Denied(_))
    ));
    assert_eq!(read(&path).unwrap(), observed);
    fs::write(
        &path,
        "[clients.mcp-stdio]\nmanage_others = true\nallow = ['configure_policy']\n",
    )
    .unwrap();
    let authorized = read(&path).unwrap();
    assert!(
        apply(&path, &authorized.revision, &defaults(42), |policy| {
            authorize(policy, &agent)
        })
        .unwrap()
        .changed
    );
}

#[test]
fn authorization_uses_current_file_and_never_a_proposed_or_previous_policy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    fs::write(&path, "[clients.mcp-stdio]\nmanage_others = true\n").unwrap();
    let prior = read(&path).unwrap();
    fs::write(&path, "[clients.mcp-stdio]\nmanage_others = false\n").unwrap();
    let current = read(&path).unwrap();
    assert!(matches!(
        apply(&path, &prior.revision, &defaults(42), |policy| authorize(
            policy,
            &Client::McpStdio
        )),
        Err(ConfigurationError::Denied(_))
    ));
    assert_eq!(read(&path).unwrap(), current);
    fs::write(&path, "[clients.cli]\ndeny = ['configure_policy']\n").unwrap();
    let denied_owner = read(&path).unwrap();
    assert!(matches!(
        apply(&path, &denied_owner.revision, &defaults(42), owner),
        Err(ConfigurationError::Denied(_))
    ));
}

#[test]
fn malformed_oversized_or_unsupported_files_are_never_replaced_with_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    for content in [
        "[defaults]\nmax_volume = 999\n".to_owned(),
        "owner = 'invented'\n".to_owned(),
        "[quiet_hours]\nstart = '22:00'\n".to_owned(),
        "#".repeat(MAX_CONFIG_BYTES + 1),
    ] {
        fs::write(&path, &content).unwrap();
        assert!(apply(&path, "unknown", &defaults(40), owner).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
    let dotted = "rooms.Bedroom.max_volume = 45\n";
    assert!(Policy::from_toml(dotted).is_ok());
    assert!(
        edit(
            dotted,
            &PolicyEdit::SetRoom {
                room: "Bedroom".into(),
                max_volume: Some(30),
                max_step: Some(5)
            }
        )
        .is_err()
    );
}

#[test]
fn invalid_values_are_refused_before_a_directory_or_lock_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent/policy.toml");
    for change in [
        defaults(101),
        PolicyEdit::SetQuietHours {
            start: "22:00".into(),
            end: "22:00".into(),
            max_volume: 20,
        },
        PolicyEdit::SetQuietHours {
            start: "9:00".into(),
            end: "07:00".into(),
            max_volume: 20,
        },
        PolicyEdit::SetRoom {
            room: "\n".into(),
            max_volume: None,
            max_step: None,
        },
        PolicyEdit::RemoveClient { client: " ".into() },
    ] {
        assert!(matches!(
            apply(&path, "observed", &change, owner),
            Err(ConfigurationError::Invalid(_))
        ));
    }
    assert!(!path.parent().unwrap().exists());
}

#[cfg(unix)]
#[test]
fn atomic_publication_and_lock_keep_policy_permissions_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("policy.toml");
    let old = read(&path).unwrap();
    let saved = apply(&path, &old.revision, &defaults(50), owner).unwrap();
    assert_eq!(read(&path).unwrap(), saved.snapshot);
    for file in [&path, &path.with_extension("toml.lock")] {
        assert_eq!(
            fs::metadata(file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}
