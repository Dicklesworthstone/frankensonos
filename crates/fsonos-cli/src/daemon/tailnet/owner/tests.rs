use super::*;
use fsonos_tailscale::Tailnet;
use std::cell::RefCell;

const OWNER: &str = "owner@example.com";

fn up() -> TailnetStatus {
    TailnetStatus::Available(Tailnet {
        source: Source::Cli,
        backend_state: Some("Running".into()),
        running: true,
        logged_in: true,
        ipv4: vec!["100.64.0.10".parse().unwrap()],
        ipv6: vec!["fd7a:115c:a1e0::10".parse().unwrap()],
        magic_dns_name: Some("speaker-host.example-tailnet.ts.net".into()),
        tailnet: Some("example-tailnet.ts.net".into()),
    })
}

fn changed(change: impl FnOnce(&mut Tailnet)) -> TailnetStatus {
    let TailnetStatus::Available(mut tailnet) = up() else {
        unreachable!()
    };
    change(&mut tailnet);
    TailnetStatus::Available(tailnet)
}

fn identity(login: &str) -> Identity {
    Identity {
        login: Some(login.into()),
        display_name: None,
        device: "speaker-host".into(),
        tags: Vec::new(),
    }
}

fn refresh(owner: &mut Owner, status: &TailnetStatus, result: Option<Identity>) {
    owner.refresh(status, |_| result.clone(), |_| {});
}

#[test]
fn a_late_login_and_reconnect_restore_owner_rights_without_a_restart() {
    let mut owner = Owner::default();
    let mut updates = Vec::new();
    owner.refresh(
        &TailnetStatus::Unavailable(Unavailable::NotInstalled),
        |_| panic!("no local status means no identity lookup"),
        |login| updates.push(login),
    );
    assert!(updates.is_empty());
    for _ in 0..2 {
        owner.refresh(
            &up(),
            |_| Some(identity(OWNER)),
            |login| updates.push(login),
        );
        assert_eq!(owner.login.as_deref(), Some(OWNER));
        let logged_out = changed(|t| {
            t.running = false;
            t.logged_in = false;
            t.backend_state = Some("NeedsLogin".into());
        });
        owner.refresh(
            &logged_out,
            |_| panic!("a logged-out host cannot establish an owner"),
            |login| updates.push(login),
        );
        assert_eq!(owner.login, None);
    }
    assert_eq!(
        updates,
        [Some(OWNER.into()), None, Some(OWNER.into()), None]
    );
}

#[test]
fn only_an_answered_local_running_identity_can_grant_ownership() {
    for status in [
        TailnetStatus::Unavailable(Unavailable::TimedOut),
        changed(|t| t.source = Source::Interfaces),
        changed(|t| t.logged_in = false),
        changed(|t| {
            t.running = false;
            t.backend_state = Some("Stopped".into());
        }),
        changed(|t| {
            t.ipv4 = vec!["192.0.2.10".parse().unwrap()];
            t.ipv6 = vec!["2001:db8::10".parse().unwrap()];
        }),
    ] {
        let mut owner = Owner::default();
        owner.refresh(
            &status,
            |_| panic!("must not query unproved addresses: {status:?}"),
            |_| panic!("must not grant an unproved identity"),
        );
        assert_eq!(owner.login, None);
    }
}

#[test]
fn a_changed_host_revokes_its_old_owner_before_a_new_lookup() {
    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Owner(Option<String>),
        Lookup,
    }
    for status in [
        changed(|t| t.ipv4 = vec!["100.64.0.11".parse().unwrap()]),
        changed(|t| t.magic_dns_name = Some("other.example-tailnet.ts.net".into())),
        changed(|t| t.tailnet = Some("other-tailnet.ts.net".into())),
    ] {
        let events = RefCell::new(Vec::new());
        let mut owner = Owner::new(&up(), Some(OWNER.into()));
        owner.refresh(
            &status,
            |_| {
                events.borrow_mut().push(Event::Lookup);
                Some(identity("new-owner@example.com"))
            },
            |login| events.borrow_mut().push(Event::Owner(login)),
        );
        assert_eq!(
            events.into_inner(),
            [
                Event::Owner(None),
                Event::Lookup,
                Event::Owner(Some("new-owner@example.com".into()))
            ]
        );
    }
}

#[test]
fn a_new_account_or_tagged_host_replaces_ownership_even_at_the_same_address() {
    let mut owner = Owner::new(&up(), Some(OWNER.into()));
    refresh(&mut owner, &up(), Some(identity("new-owner@example.com")));
    assert_eq!(owner.login.as_deref(), Some("new-owner@example.com"));
    // A tagged node never inherits a UserProfile's login as owner authority.
    let mut tagged = identity(OWNER);
    tagged.tags.push("tag:audio".into());
    refresh(&mut owner, &up(), Some(tagged));
    assert_eq!(owner.login, None);
    refresh(&mut owner, &up(), Some(identity(OWNER)));
    let mut unnamed = identity(OWNER);
    unnamed.login = None;
    refresh(&mut owner, &up(), Some(unnamed));
    assert_eq!(owner.login, None);
}

#[test]
fn brief_status_or_whois_failures_preserve_proof_but_repeated_misses_expire_it() {
    let mut owner = Owner::new(&up(), Some(OWNER.into()));
    let interfaces = changed(|t| t.source = Source::Interfaces);
    for status in [
        &interfaces,
        &up(),
        &TailnetStatus::Unavailable(Unavailable::TimedOut),
    ] {
        refresh(&mut owner, status, None);
        assert_eq!(owner.login.as_deref(), Some(OWNER));
    }
    refresh(&mut owner, &up(), Some(identity(OWNER)));
    assert_eq!(
        owner.unanswered, 0,
        "a proved identity resets the grace budget"
    );
    for _ in 1..UNANSWERED_LIMIT {
        refresh(&mut owner, &up(), None);
        assert_eq!(owner.login.as_deref(), Some(OWNER));
    }
    refresh(&mut owner, &up(), None);
    assert_eq!(owner.login, None);
    refresh(&mut owner, &up(), Some(identity(OWNER)));
    assert_eq!(owner.login.as_deref(), Some(OWNER));
    refresh(
        &mut owner,
        &TailnetStatus::Unavailable(Unavailable::Disabled),
        None,
    );
    assert_eq!(owner.login, None, "disabled detection has no owner grace");
}

#[test]
fn ipv6_only_hosts_and_ipv4_lookup_failures_can_establish_the_owner() {
    for status in [up(), changed(|t| t.ipv4.clear())] {
        let mut owner = Owner::default();
        let mut asked = Vec::new();
        owner.refresh(
            &status,
            |ip| {
                asked.push(ip);
                ip.is_ipv6().then(|| identity(OWNER))
            },
            |_| {},
        );
        assert_eq!(owner.login.as_deref(), Some(OWNER));
        assert_eq!(asked.last().unwrap().to_string(), "fd7a:115c:a1e0::10");
        assert!(asked.len() <= 2);
    }
}

#[cfg(unix)]
fn fake_probe(dir: &std::path::Path) -> Probe {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    let script = dir.join("tailscale");
    std::fs::write(
        &script,
        "#!/bin/sh\ncd -- \"$(dirname -- \"$0\")\" || exit 1\nprintf '%s\\n' \"$*\" >> calls\ncase \"$1\" in\nwhois) cat identity.json ;;\nstatus) cat status.json ;;\n*) exit 1 ;;\nesac\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    write_status(dir, &up());
    Probe {
        cli_candidates: vec![script],
        timeout: Duration::from_secs(3),
    }
}

#[cfg(unix)]
fn write_status(dir: &std::path::Path, status: &TailnetStatus) {
    let TailnetStatus::Available(status) = status else {
        panic!("fixture requires a CLI answer");
    };
    let addresses: Vec<_> = status
        .ipv4
        .iter()
        .map(ToString::to_string)
        .chain(status.ipv6.iter().map(ToString::to_string))
        .collect();
    std::fs::write(
        dir.join("status.json"),
        serde_json::json!({
            "BackendState": status.backend_state,
            "HaveNodeKey": status.logged_in,
            "TailscaleIPs": addresses,
            "Self": {"DNSName": status.magic_dns_name},
            "CurrentTailnet": {"Name": status.tailnet}
        })
        .to_string(),
    )
    .unwrap();
}

#[cfg(unix)]
fn write_identity(dir: &std::path::Path, login: &str) {
    std::fs::write(
        dir.join("identity.json"),
        serde_json::json!({
            "Node": {"Name": "speaker-host.example-tailnet.ts.net."},
            "UserProfile": {"LoginName": login}
        })
        .to_string(),
    )
    .unwrap();
}

#[cfg(unix)]
#[test]
fn every_owner_probe_reads_fresh_whois_instead_of_reusing_a_peer_cache() {
    let dir = tempfile::tempdir().unwrap();
    let probe = fake_probe(dir.path());
    let mut owner = Owner::default();
    for login in [OWNER, "new-owner@example.com"] {
        write_identity(dir.path(), login);
        owner.probe(&up(), &probe, |_| {});
        assert_eq!(owner.login.as_deref(), Some(login));
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls")).unwrap(),
        "whois --json 100.64.0.10\nstatus --json\nwhois --json 100.64.0.10\nstatus --json\n"
    );
}

#[cfg(unix)]
#[test]
fn an_account_switch_during_whois_cannot_grant_ownership_to_an_old_addresses_peer() {
    let dir = tempfile::tempdir().unwrap();
    let probe = fake_probe(dir.path());
    write_identity(dir.path(), "other-peer@example.com");
    for status in [
        changed(|t| t.ipv4 = vec!["100.64.0.11".parse().unwrap()]),
        changed(|t| t.tailnet = Some("other-tailnet.ts.net".into())),
        changed(|t| {
            t.running = false;
            t.logged_in = false;
            t.backend_state = Some("NeedsLogin".into());
        }),
    ] {
        write_status(dir.path(), &status);
        let mut owner = Owner::new(&up(), Some(OWNER.into()));
        let mut updates = Vec::new();
        owner.probe(&up(), &probe, |login| updates.push(login));
        assert_eq!(owner.login, None);
        assert_eq!(updates, [None], "no changed identity may be published");
    }
}

#[cfg(unix)]
#[test]
fn failed_status_confirmation_never_grants_and_counts_toward_proof_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let probe = fake_probe(dir.path());
    write_identity(dir.path(), "other-peer@example.com");
    std::fs::write(dir.path().join("status.json"), "not a status").unwrap();
    let mut unknown = Owner::default();
    unknown.probe(&up(), &probe, |_| {
        panic!("an unconfirmed identity must not be granted")
    });
    assert_eq!(unknown.login, None);
    let mut owner = Owner::new(&up(), Some(OWNER.into()));
    for _ in 1..UNANSWERED_LIMIT {
        owner.probe(&up(), &probe, |_| {
            panic!("brief failures retain the last proof")
        });
        assert_eq!(owner.login.as_deref(), Some(OWNER));
    }
    let mut updates = Vec::new();
    owner.probe(&up(), &probe, |login| updates.push(login));
    assert_eq!(owner.login, None);
    assert_eq!(updates, [None]);
}

#[cfg(unix)]
#[test]
fn a_tagged_or_unowned_answer_revokes_even_when_status_cannot_be_confirmed() {
    let dir = tempfile::tempdir().unwrap();
    let probe = fake_probe(dir.path());
    std::fs::write(dir.path().join("status.json"), "unavailable").unwrap();
    for response in [
        serde_json::json!({
            "Node": {"Name": "speaker-host", "Tags": ["tag:audio"]},
            "UserProfile": {"LoginName": OWNER}
        }),
        serde_json::json!({"Node": {"Name": "speaker-host"}}),
        serde_json::json!({
            "Node": {"Name": "speaker-host"},
            "UserProfile": {"LoginName": "   "}
        }),
    ] {
        std::fs::write(dir.path().join("identity.json"), response.to_string()).unwrap();
        let mut owner = Owner::new(&up(), Some(OWNER.into()));
        let mut updates = Vec::new();
        owner.probe(&up(), &probe, |login| updates.push(login));
        assert_eq!(owner.login, None);
        assert_eq!(updates, [None]);
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("calls")).unwrap(),
        "whois --json 100.64.0.10\nwhois --json 100.64.0.10\nwhois --json 100.64.0.10\n"
    );
}
