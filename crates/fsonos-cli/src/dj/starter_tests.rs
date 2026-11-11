//! Policy identities follow the real feed through partial failure and reopen.

use super::*;
use fsonos_core::HouseholdState;
use fsonos_core::clock::FakeClock;
use fsonos_core::store::MemStore;
use fsonos_proto::Transport;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_spotify::cache::apply_library_read;
use fsonos_spotify::library::LibraryItem;
use serde_json::Value;
use std::cell::Cell;
use std::net::IpAddr;

struct Rig {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    file: PathBuf,
}

impl Rig {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Den", model),
            SimPlayerSpec::new("Office", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Den").unwrap().ip;
        let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        let directory = std::env::temp_dir().join(format!(
            "fsonos-dj-starter-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        Self {
            sim,
            households: vec![household],
            coordinator,
            host,
            file: directory.join(saved::FEEDS_FILE),
        }
    }

    fn engine(&self) -> SpotifyDj {
        SpotifyDj::new(None).with_saved_feeds(Some(self.file.clone()))
    }

    fn at<'a>(&'a self, transport: &'a dyn Transport) -> DjSpeakers<'a> {
        DjSpeakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn saved(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(&self.file).unwrap()).unwrap()
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z").unwrap())
}

fn library() -> MemStore {
    let mut store = MemStore::default();
    let tracks: Vec<_> = (0..8)
        .map(|n| LibraryItem {
            source_uri: format!("spotify:track:{n:022}"),
            title: format!("Policy test song {n}"),
            artists: vec![format!("Policy test artist {n}")],
            genres: vec!["pop".into()],
            duration_secs: Some(180),
            ..LibraryItem::default()
        })
        .collect();
    apply_library_read(&mut store, &tracks).unwrap();
    store
}

#[test]
fn a_partially_failed_start_keeps_its_caller_after_process_reopen() {
    for generation in [1, 2] {
        let rig = Rig::new(generation);
        let lan = rig.sim.lan();
        let mut store = library();
        let dj = rig.engine();
        rig.sim.upnp_fault("Den", "SetPlayMode", 701).unwrap();
        assert!(
            dj.act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Start,
                &clock(),
                &Client::McpStdio
            )
            .is_err()
        );
        assert!(dj.feeds(&rig.coordinator));
        let feed = dj.state().feeds[&rig.coordinator].snapshot();
        assert!(feed.as_ref().is_some_and(|feed| !feed.queued.is_empty()));
        assert_eq!(
            rig.saved()["starters"][&rig.coordinator.0],
            serde_json::json!(["mcp-stdio"])
        );
        drop(dj);

        let reopened = rig.engine();
        assert_eq!(
            reopened.starter(&rig.coordinator),
            DjStarter::Known(vec![Client::McpStdio])
        );
        assert_eq!(reopened.state().feeds[&rig.coordinator].snapshot(), feed);
        rig.sim.clear_faults("Den").unwrap();
        let queued = browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects;
        reopened
            .act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Stop,
                &clock(),
                &Client::Cli,
            )
            .unwrap();
        assert_eq!(
            browse_snapshot(&lan, rig.host, QUEUE).unwrap().objects,
            queued
        );
        assert_eq!(reopened.starter(&rig.coordinator), DjStarter::Missing);
        drop(reopened);
        assert!(!rig.engine().feeds(&rig.coordinator));
        assert!(rig.saved().get("starters").is_none());
    }
}

#[test]
fn a_failed_restart_keeps_both_callers_even_when_the_old_queue_was_replaced() {
    for generation in [1, 2] {
        for replaced in [false, true] {
            let rig = Rig::new(generation);
            let lan = rig.sim.lan();
            let mut store = library();
            let dj = rig.engine();
            dj.act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Start,
                &clock(),
                &Client::Cli,
            )
            .unwrap();
            if replaced {
                soap::remove_all_tracks_from_queue(&lan, rig.host).unwrap();
            }
            rig.sim.upnp_fault("Den", "SetPlayMode", 701).unwrap();
            for _ in 0..2 {
                assert!(
                    dj.act_as(
                        rig.at(&lan),
                        &mut store,
                        DjAction::Start,
                        &clock(),
                        &Client::McpStdio
                    )
                    .is_err()
                );
            }
            let expected = DjStarter::Known(vec![Client::Cli, Client::McpStdio]);
            assert_eq!(dj.starter(&rig.coordinator), expected);
            assert!(
                !browse_snapshot(&lan, rig.host, QUEUE)
                    .unwrap()
                    .objects
                    .is_empty()
            );
            drop(dj);
            let reopened = rig.engine();
            assert_eq!(reopened.starter(&rig.coordinator), expected);

            rig.sim.clear_faults("Den").unwrap();
            reopened
                .act_as(
                    rig.at(&lan),
                    &mut store,
                    DjAction::Start,
                    &clock(),
                    &Client::Cli,
                )
                .unwrap();
            assert_eq!(
                reopened.starter(&rig.coordinator),
                DjStarter::Known(vec![Client::Cli])
            );
            drop(reopened);
            assert_eq!(
                rig.engine().starter(&rig.coordinator),
                DjStarter::Known(vec![Client::Cli])
            );
        }
    }
}

#[test]
fn missing_or_invalid_saved_identity_retains_the_feed_until_a_successful_explicit_start() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    dj.act_as(
        rig.at(&lan),
        &mut store,
        DjAction::Start,
        &clock(),
        &Client::McpStdio,
    )
    .unwrap();
    let original = rig.saved();
    let feed = dj.state().feeds[&rig.coordinator].snapshot();
    drop(dj);
    for invalid in [
        Value::Null,
        serde_json::json!([]),
        serde_json::json!(["cli", "\n"]),
    ] {
        let mut saved = original.clone();
        if invalid.is_null() {
            saved.as_object_mut().unwrap().remove("starters");
        } else {
            saved["starters"][&rig.coordinator.0] = invalid;
        }
        std::fs::write(&rig.file, serde_json::to_string(&saved).unwrap()).unwrap();
        let reopened = rig.engine();
        assert!(reopened.feeds(&rig.coordinator));
        assert_eq!(reopened.state().feeds[&rig.coordinator].snapshot(), feed);
        assert_eq!(reopened.starter(&rig.coordinator), DjStarter::Missing);
        rig.sim.upnp_fault("Den", "SetPlayMode", 701).unwrap();
        assert!(
            reopened
                .act_as(
                    rig.at(&lan),
                    &mut store,
                    DjAction::Start,
                    &clock(),
                    &Client::Cli
                )
                .is_err()
        );
        assert_eq!(reopened.starter(&rig.coordinator), DjStarter::Missing);
        assert!(
            reopened
                .status(rig.at(&lan), &store, None, &clock())
                .unwrap()
                .running
        );
        rig.sim.clear_faults("Den").unwrap();
        reopened
            .act_as(
                rig.at(&lan),
                &mut store,
                DjAction::Start,
                &clock(),
                &Client::McpStdio,
            )
            .unwrap();
        assert_eq!(
            reopened.starter(&rig.coordinator),
            DjStarter::Known(vec![Client::McpStdio])
        );
    }
}

#[test]
fn an_explicit_handoff_moves_policy_identity_in_the_same_saved_revision() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    let caller = Client::Tailnet("listener@example.invalid".into());
    dj.act_as(rig.at(&lan), &mut store, DjAction::Start, &clock(), &caller)
        .unwrap();
    let other = rig.sim.player("Office").unwrap();
    let to = PlayerId(other.uuid.clone());
    soap::join_group(&lan, other.ip, &rig.coordinator).unwrap();
    dj.moving(&rig.coordinator);
    soap::delegate_group_and_rejoin(&lan, rig.host, &to).unwrap();
    dj.moved(&rig.coordinator, &to);
    assert_eq!(dj.starter(&rig.coordinator), DjStarter::Missing);
    assert_eq!(dj.starter(&to), DjStarter::Known(vec![caller.clone()]));
    assert!(rig.saved()["starters"].get(&rig.coordinator.0).is_none());
    drop(dj);
    let reopened = rig.engine();
    assert!(!reopened.feeds(&rig.coordinator));
    assert!(reopened.feeds(&to));
    assert_eq!(reopened.starter(&to), DjStarter::Known(vec![caller]));
}

#[test]
fn invalid_client_keys_never_start_or_persist_a_feed() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    for client in [
        Client::Tailnet("cli".into()),
        Client::Tailnet("bad\nidentity".into()),
    ] {
        let before = rig.sim.soap_log().len();
        assert!(
            dj.act_as(rig.at(&lan), &mut store, DjAction::Start, &clock(), &client)
                .is_err()
        );
        assert_eq!(rig.sim.soap_log().len(), before);
        assert!(!dj.feeds(&rig.coordinator));
        assert!(!rig.file.exists());
    }
}

#[test]
fn an_unwritable_provenance_revision_aborts_before_engine_start_io() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    dj.act_as(
        rig.at(&lan),
        &mut store,
        DjAction::Start,
        &clock(),
        &Client::Cli,
    )
    .unwrap();
    let previous = rig.file.with_extension("previous.json");
    std::fs::rename(&rig.file, &previous).unwrap();
    std::fs::create_dir(&rig.file).unwrap();
    let before = rig.sim.soap_log().len();
    let failed = dj
        .act_as(
            rig.at(&lan),
            &mut store,
            DjAction::Start,
            &clock(),
            &Client::McpStdio,
        )
        .unwrap_err();
    assert!(failed.detail.contains("could not be saved"));
    assert_eq!(rig.sim.soap_log().len(), before);
    std::fs::remove_dir(&rig.file).unwrap();
    std::fs::rename(previous, &rig.file).unwrap();
    drop(dj);
    assert_eq!(
        rig.engine().starter(&rig.coordinator),
        DjStarter::Known(vec![Client::Cli])
    );
}

struct FailFinalSave<'a> {
    rig: &'a Rig,
    previous: PathBuf,
    failed: Cell<bool>,
}

impl Transport for FailFinalSave<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        let response = self.rig.sim.lan().soap_post(host, path, action, body)?;
        if action.ends_with("#Play\"") && !self.failed.replace(true) {
            // Preserve the pre-start durable revision, then make the final
            // replacement fail. No permission assumptions depend on the UID.
            std::fs::rename(&self.rig.file, &self.previous).unwrap();
            std::fs::create_dir(&self.rig.file).unwrap();
        }
        Ok(response)
    }
}

#[test]
fn a_failed_final_save_cannot_restore_the_previous_callers_exemption() {
    let rig = Rig::new(2);
    let lan = rig.sim.lan();
    let mut store = library();
    let dj = rig.engine();
    dj.act_as(
        rig.at(&lan),
        &mut store,
        DjAction::Start,
        &clock(),
        &Client::Cli,
    )
    .unwrap();
    let interrupted = FailFinalSave {
        rig: &rig,
        previous: rig.file.with_extension("before-start.json"),
        failed: Cell::new(false),
    };
    dj.act_as(
        rig.at(&interrupted),
        &mut store,
        DjAction::Start,
        &clock(),
        &Client::McpStdio,
    )
    .unwrap();
    assert!(interrupted.failed.get());
    assert_eq!(
        dj.starter(&rig.coordinator),
        DjStarter::Known(vec![Client::McpStdio])
    );
    std::fs::remove_dir(&rig.file).unwrap();
    std::fs::rename(&interrupted.previous, &rig.file).unwrap();
    drop(dj);
    let reopened = rig.engine();
    assert!(reopened.feeds(&rig.coordinator));
    assert_eq!(
        reopened.starter(&rig.coordinator),
        DjStarter::Known(vec![Client::Cli, Client::McpStdio]),
        "the saved pre-start union still applies the incoming caller's caps"
    );
}
