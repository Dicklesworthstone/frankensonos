use super::*;
use std::cell::Cell;
use std::net::{Ipv4Addr, SocketAddr};

fn member(h: &House) -> PlayerId {
    PlayerId(h.sim.player("SleepB").unwrap().uuid.clone())
}

fn volume_writes(h: &House, since: usize, room: &str) -> usize {
    h.sim.soap_log()[since..]
        .iter()
        .filter(|entry| entry.room == room && entry.action == "SetVolume")
        .count()
}

#[test]
fn recovery_never_restores_a_stale_member_address_onto_another_room() {
    for generation in [1, 2] {
        for reused_address in [false, true] {
            let mut h = House::with_generation(generation);
            let journal = Journal::default();
            let mut fade = h.crash(&journal, "#Pause", 1, true);
            soap::set_volume(&h.lan, h.ip("Foreign"), 0).unwrap();
            let expected = member(&h);
            if reused_address {
                let IpAddr::V4(old) = h.ip("SleepB") else {
                    panic!("synthetic IPv4 address");
                };
                h.sim.change_address("SleepB").unwrap();
                h.sim
                    .move_to("Foreign", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                    .unwrap();
            } else {
                let foreign = h.ip("Foreign");
                h.households[0]
                    .players
                    .iter_mut()
                    .find(|player| player.id == expected)
                    .unwrap()
                    .ip = foreign;
            }
            let before = h.sim.soap_log().len();
            let error = fade
                .restore(&h.lan, &h.households, &Fader::default(), &journal)
                .unwrap_err();
            assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
            assert_eq!(h.levels(), (30, 0));
            assert_eq!(soap::get_volume(&h.lan, h.ip("Foreign")).unwrap(), 0);
            assert_eq!(volume_writes(&h, before, "Foreign"), 0);
            assert_eq!(volume_writes(&h, before, "SleepB"), 0);
            assert!(journal.saved.lock().unwrap().is_some());

            // A refreshed inventory may complete the still-owned cleanup at
            // the original physical player; the foreign room stays untouched.
            h.refresh();
            fade.restore(&h.lan, &h.households, &Fader::default(), &journal)
                .unwrap();
            assert_eq!(h.levels(), (30, 20));
            assert_eq!(soap::get_volume(&h.lan, h.ip("Foreign")).unwrap(), 0);
            assert_eq!(volume_writes(&h, before, "Foreign"), 0);
            assert!(journal.saved.lock().unwrap().is_none());
        }
    }
}

#[test]
fn recovery_requires_its_coordinators_own_endpoint_before_observing_the_group() {
    for generation in [1, 2] {
        let mut h = House::with_generation(generation);
        let journal = Journal::default();
        let mut fade = h.crash(&journal, "#Pause", 1, true);
        let peer = h.ip("SleepB");
        h.households[0]
            .players
            .iter_mut()
            .find(|player| player.id == h.coordinator)
            .unwrap()
            .ip = peer;
        let before = h.sim.soap_log().len();
        assert!(matches!(
            fade.recovery(&h.lan, &h.households, now(), true),
            Err(CoreError::TargetChanged(_))
        ));
        assert!(matches!(
            fade.restore(&h.lan, &h.households, &Fader::default(), &journal),
            Err(CoreError::TargetChanged(_))
        ));
        assert_eq!(h.levels(), (0, 0));
        assert_eq!(volume_writes(&h, before, "SleepA"), 0);
        assert_eq!(volume_writes(&h, before, "SleepB"), 0);
        assert!(journal.saved.lock().unwrap().is_some());
    }
}

#[derive(Debug, Clone, Copy)]
enum Takeover {
    Address,
    Volume,
    Source,
    Group,
    Boot,
    Retirement,
}

struct DuringSave<'a> {
    journal: &'a Journal,
    sim: std::cell::RefCell<&'a mut SimHandle>,
    takeover: Takeover,
    saves: Cell<usize>,
}

impl SleepCheckpoint for DuringSave<'_> {
    fn save(&self, fade: &SleepFade) -> Result<(), CoreError> {
        self.journal.save(fade)?;
        let saves = self.saves.get() + 1;
        self.saves.set(saves);
        // Save 1 marks cleanup; save 2 records its first pending level after
        // observing zero. The speaker or another controller changes meanwhile.
        if saves == 2 {
            let mut sim = self.sim.borrow_mut();
            let host = sim.player("SleepA").unwrap().ip;
            let lan = sim.lan();
            match self.takeover {
                Takeover::Address => {
                    let IpAddr::V4(old) = host else {
                        panic!("synthetic IPv4 address");
                    };
                    sim.change_address("SleepA").unwrap();
                    sim.move_to("Foreign", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .unwrap();
                }
                Takeover::Volume => soap::set_volume(&lan, host, 44).unwrap(),
                Takeover::Source => soap::set_av_transport_uri(
                    &lan,
                    host,
                    "x-rincon-mp3radio://example.invalid/new-music.mp3",
                    "",
                )
                .unwrap(),
                Takeover::Group => {
                    soap::leave_group(&lan, sim.player("SleepB").unwrap().ip).unwrap();
                }
                Takeover::Boot => sim.reboot("SleepA", Duration::ZERO).unwrap(),
                Takeover::Retirement => self.journal.complete(fade)?,
            }
        }
        Ok(())
    }

    fn complete(&self, fade: &SleepFade) -> Result<(), CoreError> {
        self.journal.complete(fade)
    }

    fn current(&self, fade: &SleepFade) -> bool {
        self.journal.current(fade)
    }
}

#[test]
fn a_durable_checkpoint_cannot_authorize_restoring_new_playback_or_levels() {
    for generation in [1, 2] {
        for takeover in [
            Takeover::Address,
            Takeover::Volume,
            Takeover::Source,
            Takeover::Group,
            Takeover::Boot,
            Takeover::Retirement,
        ] {
            let mut h = House::with_generation(generation);
            let journal = Journal::default();
            let mut fade = h.crash(&journal, "#Pause", 1, true);
            soap::set_volume(&h.lan, h.ip("Foreign"), 0).unwrap();
            let before = h.sim.soap_log().len();
            let result = {
                let checkpoint = DuringSave {
                    journal: &journal,
                    sim: std::cell::RefCell::new(&mut h.sim),
                    takeover,
                    saves: Cell::new(0),
                };
                let result = fade.restore(&h.lan, &h.households, &Fader::default(), &checkpoint);
                assert!(checkpoint.saves.get() >= 2, "{takeover:?}");
                result
            };
            if matches!(takeover, Takeover::Address) {
                assert!(matches!(result, Err(CoreError::TargetChanged(_))));
            } else {
                result.unwrap();
            }
            assert_eq!(soap::get_volume(&h.lan, h.ip("Foreign")).unwrap(), 0);
            assert_eq!(volume_writes(&h, before, "Foreign"), 0);
            if matches!(takeover, Takeover::Volume) {
                assert_eq!(h.levels(), (44, 20));
                assert_eq!(volume_writes(&h, before, "SleepA"), 1, "manual write only");
            } else {
                assert_eq!(volume_writes(&h, before, "SleepA"), 0, "{takeover:?}");
                assert_eq!(volume_writes(&h, before, "SleepB"), 0, "{takeover:?}");
            }
        }
    }
}
