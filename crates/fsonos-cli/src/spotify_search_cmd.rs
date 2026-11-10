//! Read one bounded Spotify catalog page without discovering speakers. The
//! selected daemon owns its sign-in; failures never fall back to another account.

use clap::Args;
use fsonos_api::source::CatalogKind;
use fsonos_api::{Failure, SpotifySearchDto, SpotifySearchQuery};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct SpotifySearchArgs {
    /// Words or Spotify search filters. Quote a multi-word query.
    #[arg(required = true)]
    query: Vec<String>,
    /// Exactly one kind: track, album, artist, playlist, show, or episode.
    #[arg(long, default_value = "track")]
    kind: CatalogKind,
    /// At most this many matches (1-10). Refine the query for other results.
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=10))]
    limit: u32,
}

impl SpotifySearchArgs {
    fn request(&self) -> Result<SpotifySearchQuery, Failure> {
        let request = SpotifySearchQuery {
            query: self.query.join(" "),
            kind: self.kind,
            limit: self.limit,
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn run(global: &GlobalArgs, args: &SpotifySearchArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let found: SpotifySearchDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.get(&request.path()?)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        surface.search_spotify(&Client::Cli, &request.query, request.kind, request.limit)?
    };
    found.validate_for(&request)?;
    emit(global.json, &found, SpotifySearchDto::text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    fn parsed(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(
            ["fsonos", "spotify-search"]
                .into_iter()
                .chain(args.iter().copied()),
        )
    }

    fn request(args: &[&str]) -> Result<SpotifySearchQuery, Failure> {
        let Command::SpotifySearch(args) = parsed(args).unwrap().command else {
            panic!("spotify-search")
        };
        args.request()
    }

    #[test]
    fn search_keeps_literal_query_and_defaults_to_one_page_of_tracks() {
        let request = request(&["  Björk", "&", "strings  "]).unwrap();
        assert_eq!(request.kind, CatalogKind::Track);
        assert_eq!(request.limit, 10);
        assert_eq!(
            request.path().unwrap(),
            "/spotify/search?q=Bj%C3%B6rk%20%26%20strings&kind=track&limit=10"
        );
        for kind in ["track", "album", "artist", "playlist", "show", "episode"] {
            assert_eq!(
                self::request(&["query", "--kind", kind, "--limit", "3"])
                    .unwrap()
                    .kind
                    .as_str(),
                kind
            );
        }
    }

    #[test]
    fn invalid_searches_fail_before_network_routing() {
        for args in [
            vec![],
            vec!["words", "--kind", "track,artist"],
            vec!["words", "--kind", "audiobook"],
            vec!["words", "--limit", "0"],
            vec!["words", "--limit", "11"],
        ] {
            assert!(parsed(&args).is_err());
        }
        for query in [" ".to_owned(), "a\nb".to_owned(), "é".repeat(129)] {
            assert!(request(&[&query]).is_err());
        }
    }

    #[test]
    fn direct_daemon_tailnet_and_json_modes_remain_available() {
        assert!(parsed(&["music", "--direct"]).unwrap().global.direct);
        assert!(parsed(&["music", "--daemon"]).unwrap().global.daemon);
        let cli = parsed(&[
            "music",
            "--json",
            "--daemon-url",
            "https://sonos.example.ts.net",
        ])
        .unwrap();
        assert!(cli.global.json);
        assert_eq!(
            cli.global.daemon_url.as_deref(),
            Some("https://sonos.example.ts.net")
        );
    }
}
