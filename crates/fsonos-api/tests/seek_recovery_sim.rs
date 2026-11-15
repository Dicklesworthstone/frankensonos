//! An absolute seek's time belongs to one occurrence, not a new address/track.

use fsonos_api::heal::execute_healing;
use fsonos_api::plan::TransportAction;
use fsonos_api::{Command, ErrorCode, Guard};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;
use std::sync::Arc;

struct LostSeek {
    lan: SimLan,
    writes: Cell<usize>,
}

impl Transport for LostSeek {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let lost = || ProtoError::Network {
            target: "synthetic-player".into(),
            detail: "synthetic unavailable seek observation".into(),
        };
        if action.ends_with("#GetPositionInfo\"") {
            return Err(lost());
        }
        let answer = self.lan.soap_post(host, path, action, body)?;
        if action.ends_with("#Seek\"") {
            self.writes.set(self.writes.get() + 1);
            fsonos_proto::soap::parse_response(&answer, "Seek")?;
            return Err(lost());
        }
        Ok(answer)
    }
}

#[test]
fn an_unconfirmed_absolute_seek_never_readdresses_and_repeats_its_write() {
    for s2 in [false, true] {
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        } else {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        }
        .spawn()
        .unwrap();
        let player = sim.player("Room").unwrap();
        let id = PlayerId(player.uuid.clone());
        let host = player.ip;
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        soap::add_uri_to_queue(
            &lan,
            host,
            "x-file-cifs://example.invalid/track.flac",
            "",
            false,
        )
        .unwrap();
        control::play_queue_from(&lan, &houses, &id, 1).unwrap();
        let t = LostSeek {
            lan,
            writes: Cell::new(0),
        };
        let guard = Guard {
            policy: Arc::new(Policy::default()),
            client: &Client::Cli,
            clock: &SystemClock,
        };
        let command = Command::Transport {
            member: id.clone(),
            coordinator: id,
            action: TransportAction::Seek(control::Seek::At(100)),
        };
        let error = execute_healing(&t, &houses, &guard, &command, || {
            panic!("an absolute seek must not rediscover and seek a later occurrence")
        })
        .unwrap_err();
        assert_eq!(error.code, ErrorCode::PlayerUnreachable);
        assert_eq!(t.writes.get(), 1);
        assert_eq!(
            soap::get_position_info(&t.lan, host).unwrap().position_secs,
            Some(100)
        );
    }
}
