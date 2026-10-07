//! The Spotify doctor checks against the owner's real players. Read-only:
//! one `Browse FV:2` per household. Ignored by default; on the speaker LAN
//! run `FSONOS_SEEDS=ip,ip cargo test -p fsonos-core --test live_doctor --
//! --ignored --nocapture`.

use fsonos_core::doctor::{Runner, Status, spotify};
use fsonos_core::inventory;
use fsonos_proto::net::Lan;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

#[test]
#[ignore = "needs the owner's LAN: FSONOS_SEEDS=ip,ip"]
fn spotify_checks_on_the_real_households() {
    let seeds: Vec<IpAddr> = std::env::var("FSONOS_SEEDS")
        .expect("FSONOS_SEEDS=ip,ip")
        .split(',')
        .map(|s| s.trim().parse().expect("an IP address"))
        .collect();
    let lan = Arc::new(Lan::start().unwrap());
    let survey = inventory::survey(&*lan, &seeds, Duration::from_secs(2)).unwrap();
    let mut runner = Runner::new();
    spotify::register(&mut runner, &lan, &survey.households);
    let report = runner.run().unwrap();
    println!("{}", report.render_table());
    for e in &report.entries {
        assert_ne!(e.result.status, Status::Fail, "{}: {:?}", e.id, e.result);
    }
}
