//! Shell completion end to end: `fsonos completions zsh|bash|fish` writes
//! each shell's script with its room hook, a command that lists the rooms
//! fills the room cache, `fsonos __complete-rooms` prints it, and, where bash
//! is installed, bash completes `fsonos status Liv` to the cached room.

mod e2e;

use e2e::Scenario;
use fsonos_sim::SimHousehold;
use std::path::Path;
use std::process::Command;

#[test]
fn completion_scripts_complete_room_names_from_the_cache() {
    let mut s = Scenario::start("completions");
    s.sim(SimHousehold::standard());

    let empty = s.cli("rooms-empty", &["__complete-rooms"]);
    s.check(
        "rooms-empty",
        "cli",
        "before any command lists the rooms, there is nothing to complete",
        empty.ok() && empty.stdout.is_empty(),
        &empty.stdout,
    );
    let zones = s.cli("zones", &["zones"]);
    let rooms = s.cli("rooms-cached", &["__complete-rooms"]);
    let listed: Vec<&str> = rooms.stdout.lines().collect();
    s.check(
        "rooms-cached",
        "cli",
        "after fsonos zones, __complete-rooms prints the rooms, sorted",
        zones.ok()
            && listed.contains(&"Kitchen")
            && listed.contains(&"Living Room")
            && listed.windows(2).all(|w| w[0] < w[1]),
        &rooms.stdout,
    );

    for (shell, hook) in [
        ("zsh", ":_fsonos_rooms'"),
        ("bash", "complete -F _fsonos_with_rooms"),
        ("fish", "(fsonos __complete-rooms)"),
    ] {
        let run = s.cli(&format!("script-{shell}"), &["completions", shell]);
        s.check(
            &format!("script-{shell}"),
            "cli",
            &format!("fsonos completions {shell} writes the script with its room hook"),
            run.ok() && run.stdout.contains(hook),
            format!("{:.400}", run.stdout),
        );
    }

    let script = s.cli("script-bash-file", &["completions", "bash"]).stdout;
    let file = s.dir().join("fsonos.bash");
    std::fs::write(&file, &script).expect("write the bash script");
    match bash_completes(&file, &s.dir().join("data"), &["fsonos", "status", "Liv"]) {
        Some(words) => s.check(
            "bash-completes",
            "bash",
            "bash completes `fsonos status Liv` to the cached Living Room",
            words.iter().any(|w| w == "Living Room") && !words.iter().any(|w| w.starts_with('<')),
            words.join(" | "),
        ),
        None => s.pending("bash-completes", "bash is not installed here"),
    }
    s.finish();
}

/// The words bash's completion offers for `words` (the last one being
/// completed), with the script at `script` loaded; `None` without bash.
fn bash_completes(script: &Path, data: &Path, words: &[&str]) -> Option<Vec<String>> {
    let bin = Path::new(env!("CARGO_BIN_EXE_fsonos"))
        .parent()?
        .to_path_buf();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut dirs = vec![bin];
    dirs.extend(std::env::split_paths(&path));
    let quoted: Vec<String> = words.iter().map(|w| format!("'{w}'")).collect();
    let last = words.len() - 1;
    let program = format!(
        "source '{}'; COMP_WORDS=({}); COMP_CWORD={last}; \
         _fsonos_with_rooms fsonos '{}' '{}'; printf '%s\\n' \"${{COMPREPLY[@]}}\"",
        script.display(),
        quoted.join(" "),
        words[last],
        words[last.saturating_sub(1)],
    );
    let out = Command::new("bash")
        .args(["--norc", "--noprofile", "-c", &program])
        .env("PATH", std::env::join_paths(dirs).ok()?)
        .env("FSONOS_DATA_DIR", data)
        .output()
        .ok()?;
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
    )
}
