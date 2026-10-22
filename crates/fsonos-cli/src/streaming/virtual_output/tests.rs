use super::*;
use clap::Parser;
use serde_json::json;

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    args: StreamArgs,
}

fn parse(flags: &[&str]) -> StreamArgs {
    let mut args = vec!["stream", "Room"];
    args.extend_from_slice(flags);
    Cli::try_parse_from(args).unwrap().args
}

fn sink(token: &str, module: u32) -> Value {
    json!({"name": "movie", "index": 9, "owner_module": module,
        "monitor_source": "movie.monitor", "properties": {(OWNER): token}})
}

fn source(module: u32) -> Value {
    // pactl uses monitor_source for a source's monitor-of-sink NAME as well.
    json!({"name": "movie.monitor", "owner_module": module, "monitor_source": "movie"})
}

// Independent failure injection knobs, not production state.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct Server {
    sinks: Vec<Value>,
    sources: Vec<Value>,
    calls: Vec<Vec<String>>,
    load_reply_lost: bool,
    unload_reply_lost: bool,
    ignore_unload: bool,
    monitor_missing: bool,
    not_local: bool,
}

impl Server {
    fn call(&mut self, args: &[String]) -> anyhow::Result<Vec<u8>> {
        self.calls.push(args.to_vec());
        let args: Vec<_> = args.iter().map(String::as_str).collect();
        match args.as_slice() {
            ["--format=json", "info"] => Ok(serde_json::to_vec(&json!({
                "is_local": if self.not_local { "no" } else { "yes" }
            }))?),
            ["--format=json", "list", "sinks"] => Ok(serde_json::to_vec(&self.sinks)?),
            ["--format=json", "list", "sources"] => Ok(serde_json::to_vec(&self.sources)?),
            ["load-module", "module-null-sink", ..] => {
                assert_eq!(args[2], "sink_name=movie");
                assert!(args[6].contains("frankensonos.stream.id=test-token"));
                self.sinks = vec![sink("test-token", 42)];
                if !self.monitor_missing {
                    self.sources = vec![source(42)];
                }
                if self.load_reply_lost {
                    bail!("injected lost load reply");
                }
                Ok(b"42\n".to_vec())
            }
            ["unload-module", "42"] => {
                if !self.ignore_unload {
                    self.sinks.clear();
                    self.sources.clear();
                }
                if self.unload_reply_lost {
                    bail!("injected lost unload reply");
                }
                Ok(Vec::new())
            }
            _ => panic!("unexpected command: {args:?}"),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.calls.iter().filter(|args| args[0] == action).count()
    }
}

fn lease() -> Lease {
    Lease::new("movie", "test-token".into())
}

#[test]
fn virtual_output_is_opt_in_and_conflicts_with_explicit_input_or_offline_wav() {
    let ordinary = parse(&[]);
    assert!(prepare(&ordinary).unwrap().virtual_output.is_none());
    assert_eq!(prepare(&ordinary).unwrap().input, ordinary.input);
    for extra in [
        ["--input", "other.monitor"],
        ["--latency-wav", "aligned.wav"],
    ] {
        assert!(
            Cli::try_parse_from([
                "stream",
                "Room",
                "--virtual-output",
                "movie",
                extra[0],
                extra[1]
            ])
            .is_err()
        );
    }
}

#[test]
fn monitor_resolution_is_linux_pulse_only_and_never_modifies_the_original_arguments() {
    for format in ["pulse", "alsa", "avfoundation"] {
        let args = parse(&["--virtual-output", "movie", "--format", format]);
        let prepared = prepare(&args);
        assert_eq!(
            prepared.is_ok(),
            cfg!(target_os = "linux") && format == "pulse"
        );
        if let Ok(prepared) = prepared {
            assert_eq!(prepared.input, "movie.monitor");
        }
        assert_eq!(args.input, "@DEFAULT_MONITOR@");
    }
}

#[test]
fn names_cannot_be_defaults_indices_module_arguments_or_monitor_paths() {
    for name in [
        "",
        "default",
        "12",
        "@DEFAULT_SINK@",
        "movie.monitor",
        "x y",
        "x\"",
        "x;cmd",
        "x\0",
    ] {
        assert!(!valid_name(name), "{name:?}");
    }
    assert!(!valid_name(&"a".repeat(65)));
    for name in ["movie", "Office_2", "movie-output"] {
        assert!(valid_name(name));
    }
}

#[test]
fn creates_verifies_and_unloads_only_its_module_once() {
    let mut server = Server::default();
    let mut lease = lease();
    lease.create(|args| server.call(args)).unwrap();
    assert_eq!(lease.module, Some(42));
    lease.close(|args| server.call(args)).unwrap();
    lease.close(|_| panic!("cleanup must not repeat")).unwrap();
    assert_eq!(server.count("load-module"), 1);
    assert_eq!(server.count("unload-module"), 1);
    assert!(server.calls.iter().all(|args| {
        matches!(
            args[0].as_str(),
            "--format=json" | "load-module" | "unload-module"
        )
    }));
    assert!(server.sinks.is_empty());
}

#[test]
fn preexisting_sink_or_monitor_never_authorizes_creation_or_cleanup() {
    for monitor in [false, true] {
        let mut server = Server::default();
        if monitor {
            server.sources.push(source(7));
        } else {
            server.sinks.push(sink("owner", 7));
        }
        let mut lease = lease();
        assert!(lease.create(|args| server.call(args)).is_err());
        lease.close(|_| panic!("nothing was created")).unwrap();
        assert_eq!(server.count("load-module"), 0);
    }
}

#[test]
fn remote_server_and_malformed_device_lists_fail_before_writes() {
    let mut server = Server {
        not_local: true,
        ..Server::default()
    };
    assert!(lease().create(|args| server.call(args)).is_err());
    assert_eq!(server.calls.len(), 1);
    for bad in [b"null".as_slice(), b"{}", b"[{}]", b"[{\"name\":null}]"] {
        assert!(list(bad).is_err());
    }
    let duplicates = [sink("a", 1), sink("b", 2)];
    assert!(named(&duplicates, "movie").is_err());
}

#[test]
fn lost_load_reply_is_not_replayed_and_owned_output_can_still_be_cleaned() {
    let mut server = Server {
        load_reply_lost: true,
        ..Server::default()
    };
    let mut lease = lease();
    assert!(lease.create(|args| server.call(args)).is_err());
    assert!(lease.module.is_none());
    lease.close(|args| server.call(args)).unwrap();
    assert_eq!(server.count("load-module"), 1);
    assert_eq!(server.count("unload-module"), 1);
    assert!(server.sinks.is_empty());
}

#[test]
fn missing_monitor_fails_startup_but_does_not_leave_the_owned_output_behind() {
    let mut server = Server {
        monitor_missing: true,
        ..Server::default()
    };
    let mut lease = lease();
    assert!(lease.create(|args| server.call(args)).is_err());
    lease.close(|args| server.call(args)).unwrap();
    assert_eq!(server.count("unload-module"), 1);
}

#[test]
fn replaced_sink_or_module_index_is_not_removed() {
    for (token, module) in [("new-owner", 42), ("test-token", 43)] {
        let mut server = Server::default();
        let mut lease = lease();
        lease.create(|args| server.call(args)).unwrap();
        server.sinks = vec![sink(token, module)];
        assert!(lease.close(|args| server.call(args)).is_err());
        assert_eq!(server.count("unload-module"), 0);
        lease
            .close(|_| panic!("failed cleanup must not repeat"))
            .unwrap();
    }
}

#[test]
fn lost_unload_reply_is_confirmed_by_absence_not_a_second_write() {
    let mut server = Server {
        unload_reply_lost: true,
        ..Server::default()
    };
    let mut lease = lease();
    lease.create(|args| server.call(args)).unwrap();
    lease.close(|args| server.call(args)).unwrap();
    assert_eq!(server.count("unload-module"), 1);
}

#[test]
fn an_ignored_unload_is_an_error_and_cannot_be_replayed_by_drop() {
    let mut server = Server {
        ignore_unload: true,
        ..Server::default()
    };
    let mut lease = lease();
    lease.create(|args| server.call(args)).unwrap();
    assert!(lease.close(|args| server.call(args)).is_err());
    lease.close(|_| panic!("must not replay")).unwrap();
    assert_eq!(server.count("unload-module"), 1);
}

#[test]
fn a_server_renamed_creation_can_be_removed_without_unloading_the_colliding_owner() {
    let mut server = Server::default();
    let mut lease = lease();
    lease.attempted = true;
    let owner = sink("existing-owner", 7);
    let mut ours = sink("test-token", 42);
    ours["name"] = json!("movie.2");
    server.sinks = vec![owner.clone(), ours];
    lease
        .close(|args| {
            if args[0] == "unload-module" {
                assert_eq!(args[1], "42");
                server.sinks.retain(|sink| sink["owner_module"] != 42);
                Ok(Vec::new())
            } else {
                server.call(args)
            }
        })
        .unwrap();
    assert_eq!(server.sinks, vec![owner]);
}

#[test]
fn duplicated_ownership_markers_cannot_authorize_broad_cleanup() {
    let mut server = Server::default();
    let mut lease = lease();
    lease.attempted = true;
    let mut duplicate = sink("test-token", 43);
    duplicate["name"] = json!("movie.2");
    server.sinks = vec![sink("test-token", 42), duplicate];
    assert!(lease.close(|args| server.call(args)).is_err());
    assert_eq!(server.count("unload-module"), 0);
}

#[cfg(unix)]
#[test]
fn temporary_monitor_is_resolved_before_acoustic_capture_and_probe_preflight() {
    let args = parse(&[
        "--virtual-output",
        "movie",
        "--measure-latency",
        "--sync-microphone",
        "room_mic",
        "--measure-probe",
        "movie",
        "--ffmpeg",
        "/not-launched-in-preflight",
    ]);
    if cfg!(target_os = "linux") {
        let prepared = prepare(&args).unwrap();
        assert!(
            super::super::synchronization::Prepared::from_args(&prepared)
                .unwrap()
                .is_some()
        );
    } else {
        assert!(prepare(&args).is_err());
    }
}

#[test]
fn unavailable_cleanup_observation_does_not_authorize_unload_or_replay() {
    let mut server = Server::default();
    let mut lease = lease();
    lease.create(|args| server.call(args)).unwrap();
    let mut reads = 0;
    assert!(
        lease
            .close(|args| {
                assert_eq!(args, words(&["--format=json", "list", "sinks"]));
                reads += 1;
                Ok(b"truncated-json".to_vec())
            })
            .is_err()
    );
    assert_eq!(reads, 1);
    lease
        .close(|_| panic!("cleanup was already attempted"))
        .unwrap();
    assert_eq!(server.count("unload-module"), 0);
}
