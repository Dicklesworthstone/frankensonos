//! `fsonos completions <shell>`: a completion script for zsh, bash or fish,
//! and the hidden `fsonos __complete-rooms` it calls to complete room names.
//!
//! The script is clap_complete's for the command line, plus a hook per shell
//! that completes a room argument (a room, the room to move or group to, a
//! party's lead, `--rooms`) from the room cache: `<data-dir>/rooms.json`,
//! written whenever a command lists the rooms, directly or through the
//! daemon. Completing a room never touches the network or the store.

use clap::ValueEnum;
use std::fmt::Write as _;
use std::path::Path;

/// The room cache in the data directory.
pub const ROOM_CACHE: &str = "rooms.json";

/// The shells `fsonos completions` writes for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Zsh,
    Bash,
    Fish,
}

/// Subcommands whose positional arguments name rooms.
const ROOM_COMMANDS: [&str; 14] = [
    "status",
    "favorites",
    "play",
    "pause",
    "resume",
    "next",
    "previous",
    "volume",
    "mute",
    "group",
    "ungroup",
    "move",
    "party",
    "sleep",
];

/// Remember `rooms` (names, deduplicated and sorted) for completion. A
/// cache that cannot be written only means stale completions.
pub fn remember_rooms(data_dir: &Path, rooms: impl IntoIterator<Item = String>) {
    let mut names: Vec<String> = rooms.into_iter().filter(|r| !r.trim().is_empty()).collect();
    names.sort();
    names.dedup();
    if names.is_empty() {
        return;
    }
    let Ok(text) = serde_json::to_string(&names) else {
        return;
    };
    let partial = data_dir.join(format!("{ROOM_CACHE}.partial"));
    let written = std::fs::create_dir_all(data_dir)
        .and_then(|()| std::fs::write(&partial, text))
        .and_then(|()| std::fs::rename(&partial, data_dir.join(ROOM_CACHE)));
    if let Err(e) = written {
        tracing::debug!(error = %e, "room cache not written");
    }
}

/// The cached room names, or none.
#[must_use]
pub fn cached_rooms(data_dir: &Path) -> Vec<String> {
    std::fs::read_to_string(data_dir.join(ROOM_CACHE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// `fsonos __complete-rooms`: one cached room per line.
#[must_use]
pub fn rooms_text(rooms: &[String]) -> String {
    rooms.iter().fold(String::new(), |mut out, r| {
        out.push_str(r);
        out.push('\n');
        out
    })
}

/// `fsonos completions` and `fsonos __complete-rooms`, which need neither
/// the daemon nor the speakers; `None` for any other command.
pub fn run(cli: &crate::Cli) -> Option<anyhow::Result<()>> {
    match &cli.command {
        crate::Command::Completions { shell } => {
            let mut cmd = <crate::Cli as clap::CommandFactory>::command();
            print!("{}", script(*shell, &mut cmd));
            Some(Ok(()))
        }
        crate::Command::CompleteRooms => {
            let rooms = cli
                .global
                .data_dir()
                .map(|dir| cached_rooms(&dir))
                .unwrap_or_default();
            print!("{}", rooms_text(&rooms));
            Some(Ok(()))
        }
        _ => None,
    }
}

/// The completion script for `shell`, for the command line `cmd`.
#[must_use]
pub fn script(shell: Shell, cmd: &mut clap::Command) -> String {
    let mut out = Vec::new();
    let generator = match shell {
        Shell::Zsh => clap_complete::Shell::Zsh,
        Shell::Bash => clap_complete::Shell::Bash,
        Shell::Fish => clap_complete::Shell::Fish,
    };
    clap_complete::generate(generator, cmd, "fsonos", &mut out);
    let generated = String::from_utf8_lossy(&out).into_owned();
    match shell {
        Shell::Zsh => zsh(&generated),
        Shell::Bash => bash(generated),
        Shell::Fish => fish(generated),
    }
}

/// zsh: room arguments complete with `_fsonos_rooms` instead of files.
fn zsh(generated: &str) -> String {
    let rooms = |line: &str| {
        let t = line.trim_start();
        [
            "'zone",
            "':zone",
            "'::zone",
            "':to",
            "':target",
            "'::target",
        ]
        .iter()
        .any(|p| t.starts_with(p))
            || t.contains("--rooms=")
    };
    let body: Vec<String> = generated
        .lines()
        .map(|line| {
            if rooms(line) && line.contains(":_default'") {
                line.replacen(":_default'", ":_fsonos_rooms'", 1)
            } else {
                line.to_string()
            }
        })
        .collect();
    // `#compdef` must stay the file's first line.
    let (first, rest) = body
        .split_first()
        .map_or(("", &[][..]), |(f, r)| (f.as_str(), r));
    let mut out = format!("{first}\n\n");
    out.push_str(
        "_fsonos_rooms() {\n    local -a rooms\n    rooms=(${(f)\"$(fsonos __complete-rooms 2>/dev/null)\"})\n    compadd -a rooms\n}\n\n",
    );
    out.push_str(&rest.join("\n"));
    out.push('\n');
    out
}

/// bash: after clap's completion, offer the cached rooms for a room-taking
/// subcommand's non-flag words (and drop clap's `<ZONE>`-style placeholders).
fn bash(mut generated: String) -> String {
    let _ = write!(
        generated,
        r#"
_fsonos_with_rooms() {{
    _fsonos "$@"
    local cur="${{COMP_WORDS[COMP_CWORD]}}"
    local keep=() word
    for word in "${{COMPREPLY[@]}}"; do
        [[ $word == \<*\> ]] || keep+=("$word")
    done
    COMPREPLY=("${{keep[@]}}")
    if [[ $COMP_CWORD -ge 2 && $cur != -* ]]; then
        case " {commands} " in
            *" ${{COMP_WORDS[1]}} "*)
                local IFS=$'\n'
                COMPREPLY+=($(compgen -W "$(fsonos __complete-rooms 2>/dev/null)" -- "$cur"))
                ;;
        esac
    fi
}}
complete -F _fsonos_with_rooms -o bashdefault -o default fsonos
"#,
        commands = ROOM_COMMANDS.join(" ")
    );
    generated
}

/// fish: room-taking subcommands, and `--rooms`, complete with the cached
/// rooms.
fn fish(mut generated: String) -> String {
    let _ = write!(
        generated,
        "\ncomplete -c fsonos -n \"__fish_seen_subcommand_from {}\" -f -a \"(fsonos __complete-rooms)\"\n\
         complete -c fsonos -n \"__fish_seen_subcommand_from say chime announce\" -l rooms -f -r -a \"(fsonos __complete-rooms)\"\n",
        ROOM_COMMANDS.join(" ")
    );
    generated
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn generated(shell: Shell) -> String {
        script(shell, &mut crate::Cli::command())
    }

    #[test]
    fn zsh_completes_room_arguments_from_the_cache() {
        let script = generated(Shell::Zsh);
        assert!(script.starts_with("#compdef fsonos\n"), "{script:.200}");
        assert!(script.contains("\n_fsonos_rooms() {"));
        // fsonos status <zone>, fsonos move <zone> <to>.
        assert!(script.matches(":_fsonos_rooms'").count() >= 10, "{script}");
        assert!(script.contains("':to"), "{script}");
    }

    #[test]
    fn bash_and_fish_add_the_room_hook() {
        let bash = generated(Shell::Bash);
        assert!(
            bash.contains("complete -F _fsonos_with_rooms"),
            "{bash:.300}"
        );
        assert!(bash.contains("fsonos __complete-rooms"));
        let fish = generated(Shell::Fish);
        assert!(
            fish.contains("__fish_seen_subcommand_from status"),
            "{fish:.300}"
        );
        assert!(fish.contains("(fsonos __complete-rooms)"));
    }

    #[test]
    fn the_room_cache_round_trips_sorted_and_unique() {
        let dir = std::env::temp_dir().join(format!("fsonos-room-cache-{}", std::process::id()));
        remember_rooms(
            &dir,
            ["Office", "Living Room", "Kitchen", "Office", " "].map(String::from),
        );
        assert_eq!(cached_rooms(&dir), ["Kitchen", "Living Room", "Office"]);
        assert_eq!(
            rooms_text(&cached_rooms(&dir)),
            "Kitchen\nLiving Room\nOffice\n"
        );
        // Nothing to remember keeps the last list.
        remember_rooms(&dir, Vec::<String>::new());
        assert_eq!(cached_rooms(&dir).len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(cached_rooms(&dir).is_empty());
    }
}
