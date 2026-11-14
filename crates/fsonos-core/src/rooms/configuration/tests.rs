use super::*;
use std::sync::{Arc, Barrier};

fn alias(name: &str, rooms: &[&str]) -> AliasEdit {
    AliasEdit::SetAlias {
        name: name.into(),
        rooms: rooms.iter().map(|room| (*room).into()).collect(),
    }
}

fn default(client: Option<&str>, room: &str) -> AliasEdit {
    AliasEdit::SetDefault {
        client: client.map(str::to_owned),
        room: room.into(),
    }
}

#[test]
fn absent_and_empty_files_have_distinct_revisions_and_absent_no_ops_stay_absent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    let absent = read(&path).unwrap();
    assert_eq!(absent.aliases, Aliases::default());
    for change in [
        AliasEdit::RemoveAlias {
            name: "missing".into(),
        },
        AliasEdit::ClearDefault { client: None },
        AliasEdit::ClearDefault {
            client: Some("owner@example.com".into()),
        },
    ] {
        let saved = apply(&path, &absent.revision, &change).unwrap();
        assert!(!saved.changed);
        assert_eq!(saved.snapshot, absent);
        assert!(!path.exists());
    }
    fs::write(&path, "").unwrap();
    let empty = read(&path).unwrap();
    assert_eq!(empty.aliases, absent.aliases);
    assert_ne!(empty.revision, absent.revision);
    assert!(matches!(
        apply(&path, &absent.revision, &alias("work", &["Office"])),
        Err(ConfigurationError::Conflict { .. })
    ));
    assert_eq!(fs::read(&path).unwrap(), b"");
}

#[test]
fn replacements_preserve_unaffected_bytes_comments_and_multiline_value_boundaries() {
    let before = "# Owner's words\r\n[ aliases ] # catalogue\r\n'Morning rooms' = [\r\n  \"Den]@S2\", # a literal closing bracket\r\n  \"Kitchen\",\r\n] # set note\r\nkeep = 'Office'  # untouched\r\n\r\n[defaults] # fallback\r\nroom = 'Office' # room note\r\n";
    let after = edit(before, &alias("MORNING ROOMS", &["Bedroom", "Office"])).unwrap();
    assert!(after.starts_with("# Owner's words\r\n[ aliases ] # catalogue\r\n'Morning rooms' = "));
    assert!(after.contains(" # set note\r\nkeep = 'Office'  # untouched\r\n"));
    assert!(after.ends_with("\r\n[defaults] # fallback\r\nroom = 'Office' # room note\r\n"));
    let aliases = Aliases::parse(&after).unwrap();
    assert_eq!(aliases.get("morning rooms").unwrap().name, "Morning rooms");
    assert_eq!(
        aliases.get("morning rooms").unwrap().rooms,
        ["Bedroom", "Office"]
    );
    let removed = edit(
        &after,
        &AliasEdit::RemoveAlias {
            name: "morning rooms".into(),
        },
    )
    .unwrap();
    assert!(!removed.contains("Morning rooms"));
    assert!(removed.contains("keep = 'Office'  # untouched\r\n"));
    assert_eq!(
        Aliases::parse(&removed).unwrap().global_default_room(),
        Some("Office")
    );
}

#[test]
fn semantic_no_ops_retain_exact_source_while_duplicates_and_written_names_survive() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    let before =
        "# no final newline\n[aliases]\n\"Ada's\" = [\"Den\", \"Den\"] # duplicates are retained";
    fs::write(&path, before).unwrap();
    let observed = read(&path).unwrap();
    let saved = apply(&path, &observed.revision, &alias("ADA’S", &["Den", "Den"])).unwrap();
    assert!(!saved.changed);
    assert_eq!(saved.snapshot, observed);
    assert_eq!(fs::read_to_string(&path).unwrap(), before);
    let changed = apply(
        &path,
        &observed.revision,
        &alias("ada’s", &["Office", "Office"]),
    )
    .unwrap();
    assert!(changed.changed);
    let entry = changed.snapshot.aliases.get("ada's").unwrap();
    assert_eq!(entry.name, "Ada's");
    assert_eq!(entry.rooms, ["Office", "Office"]);
    assert_eq!(read(&path).unwrap(), changed.snapshot);
}

#[test]
fn explicit_defaults_are_independent_and_clearing_a_client_restores_global_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    let mut saved = read(&path).unwrap();
    for change in [
        alias("morning", &["Kitchen@S2", "Patio@S1"]),
        default(None, "Office"),
        default(Some("owner@example.com"), "Bedroom"),
        default(Some("tag:tablet"), "Kitchen@S2"),
    ] {
        saved = apply(&path, &saved.revision, &change).unwrap().snapshot;
    }
    assert_eq!(saved.aliases.global_default_room(), Some("Office"));
    assert_eq!(
        saved.aliases.client_defaults().collect::<Vec<_>>(),
        [
            ("owner@example.com", "Bedroom"),
            ("tag:tablet", "Kitchen@S2")
        ]
    );
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("# retained after client removal\n");
    fs::write(&path, text).unwrap();
    saved = read(&path).unwrap();
    let cleared = apply(
        &path,
        &saved.revision,
        &AliasEdit::ClearDefault {
            client: Some("tag:tablet".into()),
        },
    )
    .unwrap();
    assert!(cleared.changed);
    assert_eq!(
        cleared.snapshot.aliases.default_room(Some("tag:tablet")),
        Some("Office")
    );
    assert_eq!(
        cleared
            .snapshot
            .aliases
            .default_room(Some("owner@example.com")),
        Some("Bedroom")
    );
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("# retained after client removal\n")
    );
    let global = apply(
        &path,
        &cleared.snapshot.revision,
        &AliasEdit::ClearDefault { client: None },
    )
    .unwrap();
    assert_eq!(
        global.snapshot.aliases.default_room(Some("tag:tablet")),
        None
    );
    assert_eq!(
        global
            .snapshot
            .aliases
            .default_room(Some("owner@example.com")),
        Some("Bedroom")
    );
    assert_eq!(read(&path).unwrap(), global.snapshot);
}

#[test]
fn quoted_keys_and_header_text_inside_a_multiline_room_cannot_confuse_the_editor() {
    let before = "[aliases]\n'odd = # name' = 'Den' # literal key\nlong = '''Room\n[defaults]\nroom = \"fake\"\n'''\n\n[defaults] # real table\nroom = 'Office' # real default\n";
    let changed = edit(before, &alias("ODD = # NAME", &["Bedroom"])).unwrap();
    assert!(changed.contains("'odd = # name' = \"Bedroom\" # literal key"));
    let changed = edit(&changed, &alias("new room set", &["Kitchen"])).unwrap();
    assert_eq!(
        Aliases::parse(&changed)
            .unwrap()
            .get("new room set")
            .unwrap()
            .rooms,
        ["Kitchen"]
    );
    assert!(changed.contains("long = '''Room\n[defaults]\nroom = \"fake\"\n'''"));
    let changed = edit(&changed, &default(None, "Bedroom")).unwrap();
    assert!(changed.ends_with("[defaults] # real table\nroom = \"Bedroom\" # real default\n"));
    assert!(changed.contains("room = \"fake\""));
}

#[test]
fn inline_values_can_be_replaced_but_unproved_removals_and_insertions_fail_closed() {
    let inline = "# legacy layout\naliases = { desk = \"Office\", quiet = [\"Bedroom\"] }\n";
    let replaced = edit(inline, &alias("desk", &["Study"])).unwrap();
    assert_eq!(
        replaced,
        "# legacy layout\naliases = { desk = \"Study\", quiet = [\"Bedroom\"] }\n"
    );
    assert!(
        edit(
            inline,
            &AliasEdit::RemoveAlias {
                name: "desk".into()
            }
        )
        .is_err()
    );
    assert!(edit(inline, &alias("new", &["Study"])).is_err());
    let dotted = "aliases.desk = \"Office\" # existing notation\n";
    assert_eq!(
        edit(dotted, &alias("desk", &["Study"])).unwrap(),
        "aliases.desk = \"Study\" # existing notation\n"
    );
    assert!(
        edit(
            dotted,
            &AliasEdit::RemoveAlias {
                name: "desk".into()
            }
        )
        .is_err()
    );

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    fs::write(&path, inline).unwrap();
    let observed = read(&path).unwrap();
    assert!(
        apply(
            &path,
            &observed.revision,
            &AliasEdit::RemoveAlias {
                name: "desk".into()
            }
        )
        .is_err()
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), inline);
    assert_eq!(read(&path).unwrap(), observed);
}

#[test]
fn a_comment_only_external_change_invalidates_even_an_otherwise_unchanged_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    let first = apply(
        &path,
        &read(&path).unwrap().revision,
        &alias("work", &["Office"]),
    )
    .unwrap();
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("# owner changed this outside the application\n");
    fs::write(&path, &text).unwrap();
    let observed = read(&path).unwrap();
    assert!(matches!(
        apply(&path, &first.snapshot.revision, &alias("work", &["Office"])),
        Err(ConfigurationError::Conflict { current_revision }) if current_revision == observed.revision
    ));
    assert_eq!(fs::read_to_string(&path).unwrap(), text);
}

#[test]
fn two_file_writers_cannot_both_consume_one_reviewed_revision() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    let revision = read(&path).unwrap().revision;
    let ready = Arc::new(Barrier::new(2));
    let workers: Vec<_> = ["work", "rest"]
        .into_iter()
        .map(|name| {
            let path = path.clone();
            let revision = revision.clone();
            let ready = Arc::clone(&ready);
            std::thread::spawn(move || {
                ready.wait();
                apply(&path, &revision, &alias(name, &["Office"]))
            })
        })
        .collect();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
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
    assert!(winner.changed);
    assert_eq!(read(&path).unwrap(), winner.snapshot);
    assert_eq!(Aliases::load(&path).unwrap(), winner.snapshot.aliases);
}

#[test]
fn invalid_authored_fields_and_oversized_or_malformed_documents_never_publish() {
    for change in [
        alias("here", &["Office"]),
        alias("all@S2", &["Office"]),
        alias("work", &[]),
        alias("work", &["all"]),
        alias("bad\nname", &["Office"]),
        alias("work", &[""]),
        alias(&"n".repeat(MAX_NAME_CHARS + 1), &["Office"]),
        default(None, "EVERYWHERE@S1"),
        default(Some(""), "Office"),
        default(Some(&"c".repeat(MAX_CLIENT_BYTES + 1)), "Office"),
    ] {
        assert!(change.validate().is_err(), "{change:?}");
    }
    assert!(
        alias(&"é".repeat(MAX_NAME_CHARS), &["Office"])
            .validate()
            .is_ok()
    );
    assert!(
        default(Some(&"c".repeat(MAX_CLIENT_BYTES)), "Office")
            .validate()
            .is_ok()
    );
    let many = AliasEdit::SetAlias {
        name: "many".into(),
        rooms: vec!["Office".into(); MAX_ALIAS_ROOMS + 1],
    };
    assert!(many.validate().is_err());
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("aliases.toml");
    for content in [
        "[aliases]\nwork=7\n".into(),
        format!("#{}", "x".repeat(MAX_CONFIG_BYTES)),
    ] {
        fs::write(&path, &content).unwrap();
        assert!(read(&path).is_err());
        assert!(
            apply(
                &path,
                &revision(Some(content.as_bytes())),
                &alias("work", &["Office"])
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
    fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(read(&path).is_err());
    assert_eq!(fs::read(&path).unwrap(), [0xff, 0xfe]);
}

#[test]
fn publication_failures_report_whether_any_new_document_became_visible() {
    let dir = tempfile::tempdir().unwrap();
    let blocked = dir.path().join("not-a-directory");
    fs::write(&blocked, "retained").unwrap();
    assert!(matches!(
        apply(
            &blocked.join("aliases.toml"),
            &revision(None),
            &alias("work", &["Office"])
        ),
        Err(ConfigurationError::Lock { .. })
    ));
    assert_eq!(fs::read_to_string(&blocked).unwrap(), "retained");
    let directory = dir.path().join("existing-directory");
    fs::create_dir(&directory).unwrap();
    fs::write(directory.join("retained"), "retained").unwrap();
    assert!(matches!(
        publish(&directory, b"replacement"),
        Err(ConfigurationError::Publish {
            published: false,
            ..
        })
    ));
    assert_eq!(
        fs::read_to_string(directory.join("retained")).unwrap(),
        "retained"
    );
}
