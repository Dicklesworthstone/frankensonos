//! Read one bounded Spotify catalog page without discovering speakers. The
//! selected daemon owns its sign-in; failures never fall back to another account.

use clap::Args;
use fsonos_api::source::CatalogKind;
use fsonos_api::surface::spotify_search::SpotifySearchPageDto;
use fsonos_api::{Failure, SpotifySearchQuery};
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
    /// At most this many provider rows per page (1-10).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=10))]
    limit: u32,
    /// Continue the same query/kind/limit at the previous response's next_offset.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(u32).range(0..=1000))]
    offset: u32,
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
    let found: SpotifySearchPageDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.get(&request.path_at(args.offset)?)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        surface.search_spotify_page(&Client::Cli, &request, args.offset)?
    };
    found.validate_for(&request, args.offset)?;
    emit(global.json, &found, SpotifySearchPageDto::text)
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
            vec!["words", "--offset", "-1"],
            vec!["words", "--offset", "1001"],
        ] {
            assert!(parsed(&args).is_err());
        }
        for query in [" ".to_owned(), "a\nb".to_owned(), "é".repeat(129)] {
            assert!(request(&[&query]).is_err());
        }
    }

    #[test]
    fn explicit_pages_keep_the_same_query_kind_and_limit() {
        let Command::SpotifySearch(args) =
            parsed(&["music", "--kind", "album", "--limit", "3", "--offset", "7"])
                .unwrap()
                .command
        else {
            panic!("spotify-search")
        };
        assert_eq!(args.offset, 7);
        assert_eq!(
            args.request().unwrap().path_at(args.offset).unwrap(),
            "/spotify/search?q=music&kind=album&limit=3&offset=7"
        );
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
