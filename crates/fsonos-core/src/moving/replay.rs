//! Prepare a relocation without changing either player. The complete queue and
//! all household-specific render parameters must be available before the first
//! destructive operation; an unplayable item must not silently shift positions.

use super::{MoveError, ip};
use crate::snapshot::{SnapshotSource, ZoneSnapshot};
use crate::{HouseholdState, control};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{
    DidlKind, DidlObject, SpotifyBare, SpotifyRenderParams, item_metadata, parse_didl,
    spotify_bare_didl, spotify_bare_uri, spotify_queue_uri, spotify_track_didl, spotify_track_uri,
    spotify_uri_from_renderer_uri,
};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Instant;

mod direct;
mod mode;
mod owner;
mod staging;
mod verify;

struct Item {
    uri: String,
    metadata: String,
}

enum Source {
    Nothing,
    Uri {
        item: Item,
        position: Option<u32>,
    },
    Queue {
        items: Vec<Item>,
        track: u32,
        position: u32,
    },
}

pub(super) struct Prepared {
    source: Source,
    transport: TransportState,
    /// Unknown and direct-source settings are not replaced with NORMAL.
    play_mode: Option<PlayMode>,
    /// Last point at which the source identity/position was proven stable.
    origin_verified_at: Instant,
}

impl Prepared {
    pub(super) fn new<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        snap: &ZoneSnapshot,
        target: &PlayerId,
    ) -> Result<Self, MoveError> {
        let source = match &snap.source {
            SnapshotSource::Nothing => Source::Nothing,
            SnapshotSource::Uri {
                uri,
                metadata,
                position_secs,
            } => {
                let item = if let Some(spotify) = spotify_uri_from_renderer_uri(uri)
                    .filter(|s| s.starts_with("spotify:track:") || SpotifyBare::of(s).is_some())
                {
                    let title = parse_didl(metadata)
                        .ok()
                        .and_then(|v| v.into_iter().next())
                        .map_or_default(|object| object.title);
                    let params = parameters(t, households, target)?;
                    if SpotifyBare::of(&spotify).is_some() {
                        Item {
                            uri: spotify_bare_uri(&spotify, params.sid),
                            metadata: spotify_bare_didl(&spotify, &title, params.sid),
                        }
                    } else {
                        Item {
                            uri: spotify_track_uri(&spotify, &params),
                            metadata: spotify_track_didl(&spotify, &title, &params),
                        }
                    }
                } else {
                    Item {
                        uri: uri.clone(),
                        metadata: metadata.clone(),
                    }
                };
                Source::Uri {
                    item,
                    position: *position_secs,
                }
            }
            SnapshotSource::Queue {
                track,
                position_secs,
                update_id,
            } => {
                let queue = content::browse_snapshot(t, ip(households, &snap.coordinator)?, QUEUE)?;
                if queue.update_id != *update_id {
                    return Err(MoveError::SourceChanged(
                        "the source queue changed while preparing the move".into(),
                    ));
                }
                if queue.objects.is_empty() {
                    // A cleared queue stays the source (at track 0): there
                    // is nothing to move, as with no source at all.
                    return Ok(Self {
                        source: Source::Nothing,
                        transport: snap.transport_state,
                        play_mode: None,
                        origin_verified_at: Instant::now(),
                    });
                }
                if track
                    .checked_sub(1)
                    .and_then(|p| usize::try_from(p).ok())
                    .is_none_or(|p| p >= queue.objects.len())
                {
                    return Err(MoveError::SourceChanged(
                        "the source has no selected track in its queue".into(),
                    ));
                }
                let items = queue_items(t, households, target, &queue.objects)?;
                Source::Queue {
                    items,
                    track: *track,
                    position: *position_secs,
                }
            }
        };
        let play_mode = if matches!(source, Source::Queue { .. }) {
            snap.play_mode
        } else {
            None
        };
        let mut prepared = Self {
            source,
            transport: snap.transport_state,
            play_mode,
            origin_verified_at: Instant::now(),
        };
        prepared.confirm_source(t, households, snap)?;
        Ok(prepared)
    }

    fn confirm_source<T: Transport + ?Sized>(
        &mut self,
        t: &T,
        households: &[HouseholdState],
        snap: &ZoneSnapshot,
    ) -> Result<(), MoveError> {
        if !self.is_empty() {
            let host = ip(households, &snap.coordinator)?;
            let revision = match snap.source {
                SnapshotSource::Queue { update_id, .. } => Some(update_id),
                _ => None,
            };
            if let Some(expected) = self.play_mode
                && fsonos_proto::play_mode::get_play_mode(t, host)? != expected
            {
                return Err(MoveError::SourceChanged(
                    "the source queue's play mode changed during preparation".into(),
                ));
            }
            let position = verify::source(t, host, &snap.coordinator, &self.source, revision)
                .map_err(|e| match e {
                    MoveError::Core(crate::CoreError::Proto(ProtoError::Malformed(msg))) => {
                        MoveError::SourceChanged(msg)
                    }
                    other => other,
                })?;
            // Bracket the source/queue identity read with the ordering mode.
            // A controller can change shuffle/repeat immediately after the
            // first settings reply; that is still a changed source epoch and
            // must be caught before the destination is touched.
            if let Some(expected) = self.play_mode
                && fsonos_proto::play_mode::get_play_mode(t, host)? != expected
            {
                return Err(MoveError::SourceChanged(
                    "the source queue's play mode changed during preparation".into(),
                ));
            }
            self.transport = soap::get_transport_info(t, host)?.state;
            if !matches!(
                self.transport,
                TransportState::Playing | TransportState::Paused | TransportState::Stopped
            ) {
                return Err(MoveError::SourceChanged(
                    "the source is still changing transport state; retry the move".into(),
                ));
            }
            // Queue reads and render discovery take time. Use the latest
            // position of the same verified track, not the older snapshot.
            match &mut self.source {
                Source::Queue { position: at, .. } => {
                    *at = position.position_secs.unwrap_or(*at);
                }
                Source::Uri { position: at, .. } if at.is_some() => {
                    *at = position.position_secs.or(*at);
                }
                _ => {}
            }
            self.origin_verified_at = Instant::now();
        }
        Ok(())
    }

    /// Destination preparation can take long enough for a newer owner action
    /// to land on the source. Before releasing it, prove that its source,
    /// selected queue occurrence, queue revision, transport state and ordering
    /// mode still match the playback we copied. A failed read is uncertainty,
    /// never permission to send Stop.
    pub(super) fn confirm_origin<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        snap: &ZoneSnapshot,
    ) -> Result<(), MoveError> {
        if self.is_empty() {
            return Ok(());
        }
        let host = ip(households, &snap.coordinator)?;
        let revision = match snap.source {
            SnapshotSource::Queue { update_id, .. } => Some(update_id),
            _ => None,
        };
        verify::playback(
            t,
            host,
            &snap.coordinator,
            self,
            revision,
            self.origin_verified_at.elapsed(),
        )
        .map_err(|error| match error {
            MoveError::Core(crate::CoreError::Proto(ProtoError::Malformed(message))) => {
                MoveError::SourceChanged(message)
            }
            other => other,
        })?;
        if soap::get_transport_info(t, host)?.state != self.transport {
            return Err(MoveError::SourceChanged(
                "the source transport changed while copying playback".into(),
            ));
        }
        if let Some(expected) = self.play_mode
            && fsonos_proto::play_mode::get_play_mode(t, host)? != expected
        {
            return Err(MoveError::SourceChanged(
                "the source queue's play mode changed while copying playback".into(),
            ));
        }
        Ok(())
    }

    pub(super) fn is_empty(&self) -> bool {
        matches!(self.source, Source::Nothing)
    }

    pub(super) fn apply<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
    ) -> Result<(), MoveError> {
        match &self.source {
            Source::Nothing => Ok(()),
            Source::Uri { .. } => direct::apply(t, host, target, self),
            Source::Queue { items, .. } => {
                let (revision, owner) =
                    staging::replace(t, host, target, items, self.play_mode.is_some())?;
                staging::finish(t, host, target, self, revision, owner)
            }
        }
    }

    /// Finish relocation only after the original playback is still proven
    /// and its Stop is confirmed, without replaying an uncertain release.
    pub(super) fn release_origin<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        snap: &ZoneSnapshot,
    ) -> Result<(), MoveError> {
        let positioned = matches!(
            self.source,
            Source::Queue { .. }
                | Source::Uri {
                    position: Some(_),
                    ..
                }
        );
        direct::release(
            t,
            ip(households, &snap.coordinator)?,
            &snap.coordinator,
            positioned,
            self.play_mode.is_some(),
            || self.confirm_origin(t, households, snap),
        )
    }
}

fn parameters<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &PlayerId,
) -> Result<SpotifyRenderParams, MoveError> {
    control::spotify_params(t, households, target)?
        .ok_or_else(|| MoveError::NoRenderParams(target.0.clone()))
}

fn queue_items<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &PlayerId,
    objects: &[DidlObject],
) -> Result<Vec<Item>, MoveError> {
    let mut learned = None;
    let mut items = Vec::with_capacity(objects.len());
    for object in objects {
        let res = object
            .res
            .as_ref()
            .filter(|res| !res.uri.trim().is_empty())
            .ok_or_else(|| {
                MoveError::Unplayable("a source queue item has no playable resource".into())
            })?;
        if object.kind != DidlKind::Item {
            return Err(MoveError::Unplayable(
                "a source queue item is an unexpanded container".into(),
            ));
        }
        let item = if let Some(spotify) = spotify_uri_from_renderer_uri(&res.uri) {
            let episode = SpotifyBare::of(&spotify) == Some(SpotifyBare::Episode);
            if !spotify.starts_with("spotify:track:") && !episode {
                return Err(MoveError::Unplayable(
                    "a source queue item is not an expanded Spotify track or episode".into(),
                ));
            }
            if learned.is_none() {
                learned = Some(parameters(t, households, target)?);
            }
            let params = learned.as_ref().expect("parameters learned above");
            if episode {
                Item {
                    uri: spotify_bare_uri(&spotify, params.sid),
                    metadata: spotify_bare_didl(&spotify, &object.title, params.sid),
                }
            } else {
                Item {
                    uri: spotify_queue_uri(&spotify),
                    metadata: spotify_track_didl(&spotify, &object.title, params),
                }
            }
        } else {
            Item {
                uri: res.uri.clone(),
                metadata: item_metadata(object),
            }
        };
        items.push(item);
    }
    Ok(items)
}

fn invalid(message: &str) -> MoveError {
    ProtoError::Malformed(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_metadata_round_trips_resource_and_service_fields() {
        let input = r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
            xmlns:dc="http://purl.org/dc/elements/1.1/"
            xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
            <item id="track&amp;1" parentID="Q:0" restricted="true">
            <dc:title>One &amp; Two</dc:title><dc:creator>Test Artist</dc:creator>
            <upnp:class>object.item.audioItem.musicTrack</upnp:class>
            <upnp:album>Test Album</upnp:album><upnp:albumArtURI>http://example.invalid/art</upnp:albumArtURI>
            <res protocolInfo="http-get:*:audio/flac:*" duration="0:03:00">x-file-cifs://example.invalid/a.flac</res>
            <desc id="cdudn" nameSpace="urn:test">synthetic-service</desc>
            </item></DIDL-Lite>"#;
        let original = parse_didl(input).unwrap().remove(0);
        assert_eq!(parse_didl(&item_metadata(&original)).unwrap(), [original]);
    }
}

#[cfg(test)]
mod simulator_tests {
    use super::*;
    use crate::moving::{MoveMethod, copy_playback};
    use crate::{resolve_room, snapshot};
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};

    fn fixture() -> SimHandle {
        SimHousehold::builder()
            .s1([
                SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
                SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            ])
            .s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Bedroom", SimModel::One),
            ])
            .spawn()
            .unwrap()
    }

    fn houses(sim: &SimHandle) -> Vec<HouseholdState> {
        ["Kitchen", "Den"]
            .into_iter()
            .map(|room| {
                let mut household = HouseholdState::default();
                let topology =
                    get_zone_group_state(&sim.lan(), sim.player(room).unwrap().ip).unwrap();
                household.apply_topology(&topology);
                household
            })
            .collect()
    }

    fn seed_spotify(sim: &SimHandle, room: &str) {
        let households = houses(sim);
        let target = resolve_room(&households, room).unwrap();
        let lan = sim.lan();
        control::queue_spotify_tracks(
            &lan,
            &households,
            &target.coordinator.id,
            &[
                ("spotify:track:0000000000000000000001", "First"),
                ("spotify:track:0000000000000000000002", "Second"),
            ],
        )
        .unwrap()
        .unwrap();
        control::play_queue_from(&lan, &households, &target.coordinator.id, 2).unwrap();
        soap::seek_position(&lan, target.coordinator.ip, 40).unwrap();
    }

    fn read_only_since(sim: &SimHandle, start: usize) {
        assert!(
            sim.soap_log()[start..]
                .iter()
                .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse"),
            "preparation must not change either player"
        );
    }

    #[test]
    fn copying_within_the_same_group_is_a_true_noop() {
        let sim = fixture();
        for (source, member) in [("Kitchen", "Office"), ("Den", "Bedroom")] {
            seed_spotify(&sim, source);
            let lan = sim.lan();
            let coordinator = PlayerId(sim.player(source).unwrap().uuid.clone());
            soap::join_group(&lan, sim.player(member).unwrap().ip, &coordinator).unwrap();
            let households = houses(&sim);
            let from = resolve_room(&households, source).unwrap();
            let to = resolve_room(&households, member).unwrap();
            let before = control::playback(&lan, &households, &coordinator).unwrap();
            let calls = sim.soap_log().len();
            let report = copy_playback(&lan, &households, &from, &to).unwrap();
            assert_eq!(report.method, MoveMethod::Nothing);
            assert_eq!(report.new_coordinator, coordinator);
            assert_eq!(sim.soap_log().len(), calls, "same group needs no I/O");
            assert_eq!(
                control::playback(&lan, &households, &coordinator).unwrap(),
                before
            );
        }
    }

    #[test]
    fn missing_destination_linkage_leaves_both_playing_queues_intact() {
        for (source, destination, generation) in [("Kitchen", "Den", 2), ("Den", "Kitchen", 1)] {
            let sim = fixture();
            seed_spotify(&sim, source);
            seed_spotify(&sim, destination);
            let lan = sim.lan();
            let households = houses(&sim);
            let from = resolve_room(&households, source).unwrap();
            let to = resolve_room(&households, destination).unwrap();
            let before = control::playback(&lan, &households, &to.coordinator.id).unwrap();
            let queue = content::browse_all(&lan, to.coordinator.ip, QUEUE).unwrap();
            sim.retain_favorites(generation, |_| false).unwrap();
            let calls = sim.soap_log().len();
            let result = copy_playback(&lan, &households, &from, &to);
            assert!(matches!(result, Err(MoveError::NoRenderParams(_))));
            read_only_since(&sim, calls);
            assert_eq!(
                content::browse_all(&lan, to.coordinator.ip, QUEUE).unwrap(),
                queue
            );
            assert_eq!(
                control::playback(&lan, &households, &to.coordinator.id).unwrap(),
                before
            );
            assert_eq!(
                soap::get_transport_info(&lan, from.coordinator.ip)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
        }
    }

    #[test]
    fn copying_nothing_does_not_interrupt_the_destination() {
        let sim = fixture();
        seed_spotify(&sim, "Den");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        let before = control::playback(&lan, &households, &to.coordinator.id).unwrap();
        let calls = sim.soap_log().len();
        let report = copy_playback(&lan, &households, &from, &to).unwrap();
        assert_eq!(report.method, MoveMethod::Nothing);
        assert_eq!(report.new_coordinator, from.coordinator.id);
        read_only_since(&sim, calls);
        assert_eq!(
            control::playback(&lan, &households, &to.coordinator.id).unwrap(),
            before
        );
    }

    #[test]
    fn copying_a_cleared_queue_does_not_interrupt_the_destination() {
        let sim = fixture();
        seed_spotify(&sim, "Kitchen");
        seed_spotify(&sim, "Den");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        // The owner clears Kitchen's queue: its source is still the queue.
        soap::remove_all_tracks_from_queue(&lan, from.coordinator.ip).unwrap();
        let before = control::playback(&lan, &households, &to.coordinator.id).unwrap();
        let calls = sim.soap_log().len();
        let report = copy_playback(&lan, &households, &from, &to).unwrap();
        assert_eq!(report.method, MoveMethod::Nothing);
        assert_eq!(report.new_coordinator, from.coordinator.id);
        read_only_since(&sim, calls);
        assert_eq!(
            control::playback(&lan, &households, &to.coordinator.id).unwrap(),
            before
        );
    }

    #[test]
    fn changed_queue_revision_is_rejected_before_destination_mutation() {
        let sim = fixture();
        seed_spotify(&sim, "Kitchen");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        let snap = snapshot::capture(&lan, &households, &from.coordinator.id, 0).unwrap();
        soap::add_uri_to_queue(
            &lan,
            from.coordinator.ip,
            "x-file-cifs://example.invalid/new.flac",
            "",
            false,
        )
        .unwrap();
        let calls = sim.soap_log().len();
        let Err(err) = Prepared::new(&lan, &households, &snap, &to.coordinator.id) else {
            panic!("expected SourceChanged");
        };
        assert!(matches!(err, MoveError::SourceChanged(_)), "{err:?}");
        read_only_since(&sim, calls);
        assert!(
            content::browse_all(&lan, to.coordinator.ip, QUEUE)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn missing_resources_are_not_silently_dropped_from_a_queue() {
        let sim = fixture();
        let households = houses(&sim);
        let to = resolve_room(&households, "Den").unwrap();
        let objects = parse_didl(
            r#"<DIDL-Lite><item id="one"><res>x-file-cifs://example.invalid/one.flac</res></item>
            <item id="two"><res/></item></DIDL-Lite>"#,
        )
        .unwrap();
        let calls = sim.soap_log().len();
        assert!(queue_items(&sim.lan(), &households, &to.coordinator.id, &objects).is_err());
        assert_eq!(sim.soap_log().len(), calls);
    }

    #[test]
    fn native_queue_copy_preserves_metadata_and_the_selected_track() {
        let sim = fixture();
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        for n in 1..=2 {
            let uri = format!("x-file-cifs://example.invalid/track{n}.flac");
            let metadata = format!(
                "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                 xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
                 <item id=\"native{n}\" parentID=\"-1\" restricted=\"true\">\
                 <dc:title>Track {n}</dc:title><dc:creator>Test Artist</dc:creator>\
                 <upnp:class>object.item.audioItem.musicTrack</upnp:class>\
                 <upnp:album>Test Album</upnp:album>\
                 <res duration=\"0:03:00\" protocolInfo=\"http-get:*:audio/flac:*\">{uri}</res>\
                 </item></DIDL-Lite>"
            );
            soap::add_uri_to_queue(&lan, from.coordinator.ip, &uri, &metadata, false).unwrap();
        }
        control::play_queue_from(&lan, &households, &from.coordinator.id, 2).unwrap();
        soap::seek_position(&lan, from.coordinator.ip, 41).unwrap();
        let originals = content::browse_all(&lan, from.coordinator.ip, QUEUE).unwrap();
        let calls = sim.soap_log().len();
        let report = copy_playback(&lan, &households, &from, &to).unwrap();
        assert_eq!(report.method, MoveMethod::Copied);
        let playback = control::playback(&lan, &households, &to.coordinator.id).unwrap();
        assert_eq!(playback.transport.state, TransportState::Playing);
        assert_eq!(playback.position.track, 2);
        assert_eq!(playback.position.position_secs, Some(41));
        assert_eq!(
            playback.position.uri,
            "x-file-cifs://example.invalid/track2.flac"
        );
        let log = sim.soap_log();
        let added: Vec<_> = log[calls..]
            .iter()
            .filter(|entry| entry.action == "AddURIToQueue")
            .collect();
        assert_eq!(added.len(), originals.len());
        for (entry, original) in added.iter().zip(originals) {
            let metadata = &entry
                .args
                .iter()
                .find(|(key, _)| key == "EnqueuedURIMetaData")
                .unwrap()
                .1;
            assert_eq!(parse_didl(metadata).unwrap(), [original]);
        }
    }

    #[test]
    fn ignored_destination_commands_never_stop_the_source() {
        for (from_room, to_room) in [("Kitchen", "Den"), ("Den", "Kitchen")] {
            for action in ["RemoveTrackRangeFromQueue", "Seek", "Play"] {
                let sim = fixture();
                seed_spotify(&sim, from_room);
                if action == "RemoveTrackRangeFromQueue" {
                    seed_spotify(&sim, to_room);
                }
                let lan = sim.lan();
                let households = houses(&sim);
                let from = resolve_room(&households, from_room).unwrap();
                let to = resolve_room(&households, to_room).unwrap();
                sim.ignore_action(to_room, action).unwrap();
                let calls = sim.soap_log().len();
                assert!(
                    copy_playback(&lan, &households, &from, &to).is_err(),
                    "{action}"
                );
                let log = sim.soap_log();
                assert!(
                    log[calls..]
                        .iter()
                        .all(|entry| entry.room != from_room || entry.action != "Stop"),
                    "{action}: the original music must not be stopped"
                );
                if action == "RemoveTrackRangeFromQueue" {
                    assert_eq!(
                        log[calls..]
                            .iter()
                            .filter(|entry| entry.action == "AddURIToQueue")
                            .count(),
                        2,
                        "the incoming queue is staged before committing the replacement"
                    );
                    assert!(log[calls..].iter().all(|entry| {
                        entry.action != "RemoveAllTracksFromQueue"
                            && (entry.room != to_room || entry.action != "Play")
                    }));
                }
                assert_eq!(
                    soap::get_transport_info(&lan, from.coordinator.ip)
                        .unwrap()
                        .state,
                    TransportState::Playing,
                    "{action}"
                );
            }
        }
    }

    #[test]
    fn paused_sources_are_loaded_silently_at_the_selected_position() {
        for (from_room, to_room) in [("Kitchen", "Den"), ("Den", "Kitchen")] {
            let sim = fixture();
            seed_spotify(&sim, from_room);
            let lan = sim.lan();
            let households = houses(&sim);
            let from = resolve_room(&households, from_room).unwrap();
            let to = resolve_room(&households, to_room).unwrap();
            soap::pause(&lan, from.coordinator.ip).unwrap();
            let calls = sim.soap_log().len();
            copy_playback(&lan, &households, &from, &to).unwrap();
            assert!(
                sim.soap_log()[calls..]
                    .iter()
                    .all(|entry| entry.action != "Play")
            );
            let playback = control::playback(&lan, &households, &to.coordinator.id).unwrap();
            assert_eq!(playback.transport.state, TransportState::Stopped);
            assert_eq!(playback.position.track, 2);
            assert_eq!(playback.position.position_secs, Some(40));
            soap::play(&lan, to.coordinator.ip).unwrap();
            assert_eq!(
                soap::get_position_info(&lan, to.coordinator.ip)
                    .unwrap()
                    .track,
                2
            );
        }
    }

    #[test]
    fn destination_readback_failure_never_reports_a_successful_move() {
        let sim = fixture();
        seed_spotify(&sim, "Kitchen");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        sim.upnp_fault("Den", "GetPositionInfo", 701).unwrap();
        let calls = sim.soap_log().len();
        assert!(copy_playback(&lan, &households, &from, &to).is_err());
        assert!(
            sim.soap_log()[calls..]
                .iter()
                .all(|entry| entry.room != "Kitchen" || entry.action != "Stop")
        );
        assert_eq!(
            soap::get_transport_info(&lan, from.coordinator.ip)
                .unwrap()
                .state,
            TransportState::Playing
        );
    }

    #[test]
    fn a_source_track_changed_during_preparation_is_not_replayed() {
        let sim = fixture();
        seed_spotify(&sim, "Kitchen");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        let snap = snapshot::capture(&lan, &households, &from.coordinator.id, 0).unwrap();
        soap::seek_track(&lan, from.coordinator.ip, 1).unwrap();
        let calls = sim.soap_log().len();
        let Err(err) = Prepared::new(&lan, &households, &snap, &to.coordinator.id) else {
            panic!("expected SourceChanged");
        };
        assert!(matches!(err, MoveError::SourceChanged(_)), "{err:?}");
        read_only_since(&sim, calls);
    }

    #[test]
    fn unparseable_didl_metadata_falls_back_to_empty_title_on_copy() {
        let sim = fixture();
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        let (uri, metadata) = control::spotify_track_source(
            &lan,
            &households,
            &from.coordinator.id,
            "spotify:track:0000000000000000000001",
            "Broken Metadata",
        )
        .unwrap()
        .unwrap();
        control::play_uri(&lan, &households, &from.coordinator.id, &uri, &metadata).unwrap();
        let mut snap = snapshot::capture(&lan, &households, &from.coordinator.id, 0).unwrap();
        if let SnapshotSource::Uri { metadata, .. } = &mut snap.source {
            *metadata = "<DIDL-Lite><unclosed".into();
        }
        let prepared = Prepared::new(&lan, &households, &snap, &to.coordinator.id).unwrap();
        assert!(!prepared.is_empty());
    }

    #[test]
    fn source_is_stopped_only_after_destination_readback_succeeds() {
        let sim = fixture();
        seed_spotify(&sim, "Kitchen");
        let lan = sim.lan();
        let households = houses(&sim);
        let from = resolve_room(&households, "Kitchen").unwrap();
        let to = resolve_room(&households, "Den").unwrap();
        let calls = sim.soap_log().len();
        copy_playback(&lan, &households, &from, &to).unwrap();
        let log = sim.soap_log();
        let operations = &log[calls..];
        let stopped = operations
            .iter()
            .position(|entry| entry.room == "Kitchen" && entry.action == "Stop")
            .unwrap();
        let confirmed = operations
            .iter()
            .rposition(|entry| entry.room == "Den" && entry.action == "Browse")
            .unwrap();
        assert!(confirmed < stopped);
        assert_eq!(
            operations[confirmed]
                .result
                .as_ref()
                .unwrap()
                .iter()
                .find(|(key, _)| key == "TotalMatches")
                .unwrap()
                .1,
            "2"
        );
    }
}
