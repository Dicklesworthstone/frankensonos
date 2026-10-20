use super::*;
use clap::Parser;

#[derive(Parser)]
struct Harness {
    #[command(flatten)]
    serve: ServeArgs,
}

fn serve(args: &[&str]) -> ServeArgs {
    Harness::try_parse_from(std::iter::once("serve").chain(args.iter().copied()))
        .unwrap()
        .serve
}

fn job() -> Job {
    Job {
        program: vec![
            "/Users/u/My Tools/fsonos".to_owned(),
            "serve".to_owned(),
            "--seed".to_owned(),
            "192.0.2.10".to_owned(),
        ],
        env: vec![
            ("HOME".to_owned(), "/Users/u".to_owned()),
            (
                "FSONOS_DATA_DIR".to_owned(),
                "/Users/u/Library/Application Support/fsonos".to_owned(),
            ),
            (
                "FSONOS_SPOTIFY_CLIENT_ID".to_owned(),
                "a&b<c>\"d\"%e$f".to_owned(),
            ),
        ],
        home: PathBuf::from("/Users/u"),
        log: PathBuf::from("/Users/u/Library/Logs/fsonos/fsonos.log"),
        stop_secs: 90,
    }
}

/// Every variable `serve` reads, set by its flag, and nothing for a default.
#[test]
fn the_service_gets_the_settings_serve_would_get_here() {
    // Clap reads these from the environment too: keep this test's own out.
    for key in [
        "FSONOS_HTTP_ADDR",
        "FSONOS_MCP_HTTP_ADDR",
        "FSONOS_TAILSCALE",
        "FSONOS_TAILSCALE_SERVE",
        "FSONOS_SPOTIFY_CLIENT_ID",
        "FSONOS_SPOTIFY_REDIRECT_URI",
        "FSONOS_EVENTS_PORT",
    ] {
        assert!(std::env::var_os(key).is_none(), "{key} is set for the test");
    }
    assert_eq!(serve_env(&serve(&[])), Vec::new());
    let env = serve_env(&serve(&[
        "--http",
        "127.0.0.1:8099",
        "--mcp-http",
        "127.0.0.1:8098",
        "--tailscale",
        "off",
        "--tailscale-serve",
        "--spotify-client-id",
        "client",
        "--spotify-redirect-uri",
        "http://127.0.0.1:9000/cb",
        "--events-port",
        "0",
    ]));
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(
        pairs,
        [
            ("FSONOS_HTTP_ADDR", "127.0.0.1:8099"),
            ("FSONOS_MCP_HTTP_ADDR", "127.0.0.1:8098"),
            ("FSONOS_TAILSCALE", "off"),
            ("FSONOS_TAILSCALE_SERVE", "true"),
            ("FSONOS_SPOTIFY_CLIENT_ID", "client"),
            ("FSONOS_SPOTIFY_REDIRECT_URI", "http://127.0.0.1:9000/cb"),
            ("FSONOS_EVENTS_PORT", "0"),
        ]
    );
}

#[test]
fn the_launch_agent_runs_the_binary_with_its_environment() {
    let plist = plist(&job());
    assert!(plist.contains(MARKER), "{plist}");
    assert!(plist.contains(&format!("<string>{LABEL}</string>")));
    assert!(!plist.contains("UserName"), "an agent runs as its user");
    for arg in ["/Users/u/My Tools/fsonos", "serve", "--seed", "192.0.2.10"] {
        assert!(plist.contains(&format!("<string>{arg}</string>")), "{arg}");
    }
    let data_dir = "<string>/Users/u/Library/Application Support/fsonos</string>";
    assert!(plist.contains(&format!("<key>FSONOS_DATA_DIR</key>\n\t\t{data_dir}")));
    // XML's own characters are escaped; nothing else is.
    assert!(
        plist.contains("<string>a&amp;b&lt;c&gt;\"d\"%e$f</string>"),
        "{plist}"
    );
    assert!(plist.contains("<key>ExitTimeOut</key>\n\t<integer>90</integer>"));
    assert!(plist.contains(
        "<key>StandardErrorPath</key>\n\t<string>/Users/u/Library/Logs/fsonos/fsonos.log</string>"
    ));
    // The marker comment holds no "--", which XML comments may not.
    let comment = plist
        .split("<!--")
        .nth(1)
        .unwrap()
        .split("-->")
        .next()
        .unwrap();
    assert!(!comment.contains("--"), "{comment}");
    assert_eq!(
        plist.matches("<dict>").count(),
        plist.matches("</dict>").count()
    );
}

#[test]
fn the_user_unit_quotes_what_systemd_would_read_otherwise() {
    let text = unit(&job());
    assert!(text.starts_with(&format!("# {MARKER}")), "{text}");
    assert!(
        text.contains(
            "ExecStart=\"/Users/u/My Tools/fsonos\" \"serve\" \"--seed\" \"192.0.2.10\"\n"
        )
    );
    assert!(
        text.contains(
            "Environment=\"FSONOS_DATA_DIR=/Users/u/Library/Application Support/fsonos\"\n"
        )
    );
    // Quotes and backslashes escaped, `%` (a specifier) doubled; `$` is
    // literal outside ExecStart.
    assert!(
        text.contains("Environment=\"FSONOS_SPOTIFY_CLIENT_ID=a&b<c>\\\"d\\\"%%e$f\"\n"),
        "{text}"
    );
    let mut dollar = job();
    dollar.program.push("$HOME".to_owned());
    assert!(unit(&dollar).contains("\"$$HOME\""));
    assert!(text.contains("Restart=always\n"));
    assert!(text.contains("TimeoutStopSec=90\n"));
    assert!(text.contains("WantedBy=default.target\n"));
}

#[test]
fn each_manager_keeps_its_file_where_it_looks() {
    let home = Path::new("/home/u");
    assert_eq!(
        Manager::Launchd.path(home, None),
        Path::new("/home/u/Library/LaunchAgents/io.github.dicklesworthstone.fsonos.plist")
    );
    assert_eq!(
        Manager::Systemd.path(home, None),
        Path::new("/home/u/.config/systemd/user/fsonos.service")
    );
    assert_eq!(
        Manager::Systemd.path(home, Some(Path::new("/xdg"))),
        Path::new("/xdg/systemd/user/fsonos.service")
    );
    // A relative XDG_CONFIG_HOME is invalid per the spec and ignored.
    assert_eq!(
        Manager::Systemd.path(home, Some(Path::new("rel"))),
        Path::new("/home/u/.config/systemd/user/fsonos.service")
    );
}

/// The job gets as long to stop as the daemon's drain takes, and more.
#[test]
fn a_stopping_service_has_time_to_put_the_music_back() {
    assert_eq!(crate::daemon::STOP_SECS, 90);
    let plist = plist(&job());
    assert!(plist.contains(&format!(
        "<key>ExitTimeOut</key>\n\t<integer>{}</integer>",
        crate::daemon::STOP_SECS
    )));
}
