//! `fsonos sim`: a virtual Sonos house on loopback, for trying FrankenSonos
//! (or developing an agent against it) without speakers.
//!
//! Starts `fsonos-sim` households, writes a seeds file and a routes file, and
//! prints both with the players' advertised addresses. The virtual players
//! advertise documentation addresses (`192.0.2.N`, port 1400), as real players
//! advertise LAN addresses; the routes file maps each one to the loopback
//! socket that actually serves it, plus the simulator's unicast SSDP responder,
//! so a client in another process can reach them. Runs until Ctrl-C or
//! SIGTERM, then shuts the players down. Nothing binds beyond 127.0.0.1.

use anyhow::Context as _;
use clap::ValueEnum;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// `fsonos sim` options.
#[derive(Debug, Clone, clap::Args)]
pub struct SimArgs {
    /// The virtual house to run.
    #[arg(long, value_enum, default_value_t = Scenario::TwoHouseholds)]
    pub scenario: Scenario,
    /// Write the seeds file here [default: a new temporary directory]. The
    /// routes file is written beside it as `routes.toml`.
    #[arg(long)]
    pub seeds_out: Option<PathBuf>,
}

/// Which virtual households to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scenario {
    /// An S1 household (Kitchen and Office Play:5s and a Bridge) and an S2
    /// household (Living Room One and Bedroom Play:1).
    TwoHouseholds,
    /// The S1 household only.
    S1,
    /// The S2 household only.
    S2,
}

fn s1_players() -> [SimPlayerSpec; 3] {
    [
        SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
        SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        SimPlayerSpec::new("Bridge", SimModel::Bridge),
    ]
}

fn s2_players() -> [SimPlayerSpec; 2] {
    [
        SimPlayerSpec::new("Living Room", SimModel::One),
        SimPlayerSpec::new("Bedroom", SimModel::Play1),
    ]
}

fn spawn(scenario: Scenario) -> anyhow::Result<SimHandle> {
    let builder = match scenario {
        Scenario::TwoHouseholds => SimHousehold::builder().s1(s1_players()).s2(s2_players()),
        Scenario::S1 => SimHousehold::builder().s1(s1_players()),
        Scenario::S2 => SimHousehold::builder().s2(s2_players()),
    };
    builder.spawn().context("start the virtual players")
}

/// The routes file: each player's advertised address and the loopback socket
/// serving it, and the unicast SSDP responder to search instead of multicast.
#[must_use]
pub fn routes_toml(sim: &SimHandle) -> String {
    let mut out = String::from(
        "# fsonos-sim routes: each player's advertised address -> the loopback socket \
         serving it.\n",
    );
    let _ = writeln!(
        out,
        "ssdp = \"{}\"  # unicast SSDP responder (M-SEARCH here, not multicast)",
        sim.ssdp_addr()
    );
    out.push_str("\n[routes]\n");
    for p in sim.players() {
        let _ = writeln!(out, "\"{}\" = \"{}\"", p.ip, p.addr);
    }
    out
}

/// The startup report: players, then where the seeds and routes are.
fn banner(sim: &SimHandle, seeds: &Path, routes: &Path) -> String {
    let players = sim.players();
    let mut households: Vec<&str> = players.iter().map(|p| p.household.as_str()).collect();
    households.dedup();
    let mut out = format!(
        "fsonos sim: ready, {} virtual players in {} household(s), all on 127.0.0.1\n",
        players.len(),
        households.len()
    );
    for p in players {
        let _ = writeln!(
            out,
            "  S{}  {:<12} {:<9} {} -> {}",
            p.generation,
            p.room,
            p.model.display_name(),
            p.ip,
            p.addr
        );
    }
    let (seeds, routes) = (shell_word(seeds), shell_word(routes));
    let _ = writeln!(out, "seeds:  {seeds}");
    let _ = writeln!(out, "routes: {routes}");
    let flags = format!("--seeds {seeds} --routes {routes}");
    let _ = write!(
        out,
        "Try, in another terminal:\n  \
         fsonos {flags} discover\n  \
         fsonos {flags} zones\n  \
         fsonos {flags} volume Kitchen 30\n  \
         claude mcp add fsonos-sim -- fsonos {flags} mcp\n\
         or export FSONOS_SEEDS={seeds} FSONOS_ROUTES={routes}\n\
         Ctrl-C stops the virtual players.\n"
    );
    out
}

/// `path` as one shell word (single-quoted when it needs to be).
fn shell_word(path: &Path) -> String {
    let s = path.display().to_string();
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || "/._-+:@".contains(c))
    {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// Run the virtual house until Ctrl-C / SIGTERM.
pub fn run(args: &SimArgs) -> anyhow::Result<()> {
    let (seeds, temp_dir) = if let Some(path) = &args.seeds_out {
        (path.clone(), None)
    } else {
        let dir = std::env::temp_dir().join(format!("fsonos-sim-{}", std::process::id()));
        std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
        (dir.join("seeds.toml"), Some(dir))
    };
    let routes = seeds.with_file_name("routes.toml");

    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGINT, signal_hook::consts::SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))
            .context("install the Ctrl-C handler")?;
    }

    let sim = spawn(args.scenario)?;
    std::fs::write(&seeds, sim.seeds_toml())
        .with_context(|| format!("write {}", seeds.display()))?;
    std::fs::write(&routes, routes_toml(&sim))
        .with_context(|| format!("write {}", routes.display()))?;
    print!("{}", banner(&sim, &seeds, &routes));

    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("fsonos sim: stopping the virtual players");
    sim.shutdown();
    if let Some(dir) = temp_dir {
        // Only the files this run wrote, in the directory it created.
        let _ = std::fs::remove_file(&seeds);
        let _ = std::fs::remove_file(&routes);
        let _ = std::fs::remove_dir(&dir);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_and_banner_cover_every_player_on_loopback() {
        let sim = spawn(Scenario::TwoHouseholds).unwrap();
        let routes = routes_toml(&sim);
        assert!(routes.contains(&format!("ssdp = \"{}\"", sim.ssdp_addr())));
        for p in sim.players() {
            assert!(p.addr.ip().is_loopback(), "{p:?}");
            assert!(
                routes.contains(&format!("\"{}\" = \"{}\"", p.ip, p.addr)),
                "{routes}"
            );
        }
        let text = banner(
            &sim,
            Path::new("/tmp/s/seeds.toml"),
            Path::new("/tmp/s/routes.toml"),
        );
        assert!(text.starts_with("fsonos sim: ready, 5 virtual players in 2 household(s)"));
        assert!(text.contains("Kitchen") && text.contains("Bedroom"));
        assert!(text.contains("routes: /tmp/s/routes.toml"));
        assert!(
            text.contains("fsonos --seeds /tmp/s/seeds.toml --routes /tmp/s/routes.toml discover")
        );
        sim.shutdown();
    }

    #[test]
    fn paths_become_single_shell_words() {
        assert_eq!(
            shell_word(Path::new("/tmp/fsonos-sim-1/seeds.toml")),
            "/tmp/fsonos-sim-1/seeds.toml"
        );
        assert_eq!(
            shell_word(Path::new("/My Files/seeds.toml")),
            "'/My Files/seeds.toml'"
        );
        assert_eq!(shell_word(Path::new("/it's/s.toml")), r"'/it'\''s/s.toml'");
    }

    #[test]
    fn single_household_scenarios() {
        let s1 = spawn(Scenario::S1).unwrap();
        assert!(s1.players().iter().all(|p| p.generation == 1));
        assert_eq!(s1.players().len(), 3);
        s1.shutdown();
        let s2 = spawn(Scenario::S2).unwrap();
        assert!(s2.players().iter().all(|p| p.generation == 2));
        assert_eq!(s2.players().len(), 2);
        s2.shutdown();
    }
}
