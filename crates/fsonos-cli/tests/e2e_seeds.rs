//! Finding the speakers where SSDP never answers (a network that drops
//! multicast), against the virtual households: a routes file that names no
//! SSDP responder, so every multicast search is refused, and no
//! `FSONOS_SEEDS`.
//!
//! * With nothing to go on, `fsonos zones` finds no players. Where the
//!   search is heard but nobody answers (multicast that goes nowhere), it
//!   and `fsonos discover` say what to try.
//! * The seed file `fsonos setup` writes, `seeds.toml` in the data
//!   directory, is read by default: `zones` finds both households.
//! * With that file gone, the players that survey found, remembered in the
//!   store, are tried when nothing else finds any: `zones` finds them again.

mod e2e;

use e2e::{Run, Scenario};
use fsonos_sim::SimHousehold;
use serde_json::Value;

/// How many zones `zones --json` printed.
fn zones(run: &Run) -> usize {
    serde_json::from_str::<Value>(&run.stdout)
        .ok()
        .and_then(|v| v.as_array().map(Vec::len))
        .unwrap_or(0)
}

#[test]
fn speakers_are_found_where_ssdp_never_answers() {
    let mut s = Scenario::start("seeds");
    s.sim(SimHousehold::standard());
    // The harness's routes file without its SSDP responder.
    let routes = std::fs::read_to_string(s.dir().join("routes.toml")).expect("routes.toml");
    let quiet = routes
        .lines()
        .filter(|l| !l.starts_with("ssdp"))
        .collect::<Vec<_>>()
        .join("\n");
    let quiet_routes = s.dir().join("routes-no-ssdp.toml");
    std::fs::write(&quiet_routes, &quiet).expect("write the routes file");
    let quiet_routes = quiet_routes.to_string_lossy().into_owned();
    let args = ["--routes", quiet_routes.as_str(), "zones", "--json"];
    let no_seeds = ["FSONOS_SEEDS"];

    let run = s.cli_without("nothing", &args, &no_seeds);
    s.check(
        "nothing",
        "cli",
        "with SSDP refused, no seeds and nothing remembered, zones finds no players",
        !run.ok() && zones(&run) == 0,
        format!("{run:?}"),
    );

    // The search heard but never answered: a socket that takes the
    // M-SEARCH and says nothing. Both commands say what to try instead.
    let silent = std::net::UdpSocket::bind("127.0.0.1:0").expect("a silent SSDP socket");
    let silent_routes = s.dir().join("routes-silent-ssdp.toml");
    let addr = silent.local_addr().expect("its address");
    std::fs::write(&silent_routes, format!("ssdp = \"{addr}\"\n{quiet}"))
        .expect("write the routes file");
    let silent_routes = silent_routes.to_string_lossy().into_owned();
    let hints = |text: &str| {
        [
            "--seed <ip>",
            "About My System",
            "client-isolated",
            "Local Network",
        ]
        .iter()
        .all(|hint| text.contains(hint))
    };
    let run = s.cli_without(
        "silent-zones",
        &["--routes", &silent_routes, "zones"],
        &no_seeds,
    );
    s.check(
        "silent",
        "cli",
        "no answer to the search: zones finds no players and says what to try",
        !run.ok() && hints(&run.stderr),
        format!("{run:?}"),
    );
    let run = s.cli_without(
        "silent-discover",
        &["--routes", &silent_routes, "discover"],
        &no_seeds,
    );
    s.check(
        "silent",
        "cli",
        "no answer to the search: discover says it found no players, and what to try",
        run.ok() && run.stdout.starts_with("no players found") && hints(&run.stdout),
        format!("{run:?}"),
    );
    drop(silent);

    let data = s.dir().join("data");
    std::fs::create_dir_all(&data).expect("the data directory");
    std::fs::copy(s.dir().join("seeds.toml"), data.join("seeds.toml")).expect("seeds.toml");
    let run = s.cli_without("seed-file", &args, &no_seeds);
    s.check(
        "seed-file",
        "cli",
        "zones reads the seeds.toml setup writes in the data directory",
        run.ok() && zones(&run) == 4,
        format!("{run:?}"),
    );

    std::fs::rename(data.join("seeds.toml"), data.join("seeds.toml.off")).expect("set it aside");
    let run = s.cli_without("remembered", &args, &no_seeds);
    s.check(
        "remembered",
        "cli",
        "with the seed file gone, zones finds the players the last survey found",
        run.ok() && zones(&run) == 4,
        format!("{run:?}"),
    );

    // Each step's SSDP search was refused, on purpose.
    let refused = s.take_refusals();
    s.check(
        "ssdp-refused",
        "isolation",
        "only the multicast searches were refused",
        !refused.is_empty() && refused.iter().all(|r| r.contains("SSDP")),
        refused.join("\n"),
    );
    s.finish();
}
