use super::*;
use fsonos_types::Generation;

struct Probe {
    description: String,
    unavailable: bool,
    attributes: Cell<usize>,
}

impl Transport for Probe {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        assert!(url.ends_with("/xml/device_description.xml"));
        if self.unavailable {
            return Err(ProtoError::Network {
                target: url.into(),
                detail: "input did not answer".into(),
            });
        }
        Ok(self.description.clone())
    }

    fn soap_post(
        &self,
        _: IpAddr,
        path: &str,
        action: &str,
        _: &str,
    ) -> Result<String, ProtoError> {
        assert_eq!(path, AUDIO_IN.control_path);
        assert_eq!(
            action,
            soap::soap_action_header(&AUDIO_IN, "GetAudioInputAttributes")
        );
        self.attributes.set(self.attributes.get() + 1);
        Ok(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:GetAudioInputAttributesResponse xmlns:u=\"urn:schemas-upnp-org:service:AudioIn:1\">\
             <CurrentName>Vinyl &amp; CD</CurrentName><CurrentIcon>Phono</CurrentIcon>\
             </u:GetAudioInputAttributesResponse></s:Body></s:Envelope>"
                .into(),
        )
    }
}

fn player(id: &str) -> Player {
    Player {
        id: PlayerId(id.into()),
        room_name: "Study".into(),
        ip: [192, 0, 2, 10].into(),
        model: "Play:5".into(),
        generation: Generation::S1,
    }
}

fn transport(id: &str, service: bool) -> Probe {
    let audio_in = if service {
        format!(
            "<service><serviceType>{}</serviceType></service>",
            AUDIO_IN.service_type
        )
    } else {
        String::new()
    };
    Probe {
        description: format!(
            "<root><device><UDN>uuid:{id}</UDN><modelName>Unknown future model</modelName>\
             <serviceList><service><serviceType>{}</serviceType></service>{audio_in}</serviceList>\
             </device></root>",
            AV_TRANSPORT.service_type,
        ),
        unavailable: false,
        attributes: Cell::new(0),
    }
}

#[test]
fn capability_comes_from_current_services_not_the_cached_model() {
    let player = player("input");
    let without = transport("input", false);
    assert_eq!(probe(&without, &player).unwrap(), None);
    assert_eq!(without.attributes.get(), 0);
    let with = transport("input", true);
    assert_eq!(
        probe(&with, &player).unwrap(),
        Some(Source {
            player: player.id,
            room: "Study".into(),
            name: "Vinyl & CD".into(),
            icon: "Phono".into(),
        })
    );
    assert_eq!(with.attributes.get(), 1);
}

#[test]
fn address_reuse_cannot_probe_a_replacement_players_input() {
    let transport = transport("replacement", true);
    assert!(matches!(
        probe(&transport, &player("original")),
        Err(CoreError::LineInChanged(_))
    ));
    assert_eq!(transport.attributes.get(), 0);
}

#[test]
fn an_unreadable_player_is_not_reported_as_an_absent_input() {
    let mut transport = transport("input", true);
    transport.unavailable = true;
    let household = HouseholdState {
        players: vec![player("input")],
        ..Default::default()
    };
    let catalog = list(&transport, &[household], &PlayerId("input".into())).unwrap();
    assert!(catalog.sources.is_empty());
    assert_eq!(catalog.unavailable.len(), 1);
    assert_eq!(catalog.unavailable[0].player, PlayerId("input".into()));
    assert!(
        catalog.unavailable[0]
            .detail
            .contains("input did not answer")
    );
    assert_eq!(transport.attributes.get(), 0);
}

struct NoNetwork;

impl Transport for NoNetwork {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("invalid endpoint selection must fail before network access")
    }

    fn http_get(&self, _: &str) -> Result<String, ProtoError> {
        panic!("invalid endpoint selection must fail before network access")
    }
}

#[test]
fn sources_cannot_cross_households_even_with_equal_names() {
    let destination = player("destination");
    let source = player("source");
    let houses = [
        HouseholdState {
            players: vec![destination.clone()],
            ..Default::default()
        },
        HouseholdState {
            players: vec![source.clone()],
            ..Default::default()
        },
    ];
    assert!(matches!(
        prepare(&NoNetwork, &houses, &destination.id, &source.id),
        Err(CoreError::LineInUnavailable(_))
    ));
}

#[test]
fn ambiguous_household_identity_fails_before_any_io() {
    let destination = player("destination");
    let source = player("source");
    let house = HouseholdState {
        players: vec![destination.clone(), source.clone()],
        ..Default::default()
    };
    assert!(matches!(
        prepare(
            &NoNetwork,
            &[house.clone(), house],
            &destination.id,
            &source.id
        ),
        Err(CoreError::LineInChanged(_))
    ));
}
