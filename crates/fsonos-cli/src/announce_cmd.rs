//! `fsonos say` and `fsonos chime`: an announcement from the command line.
//!
//! In direct mode the CLI serves the clip itself, for as long as the
//! announcement takes, from a listener on the address it uses to reach the
//! speakers (the address `fsonos serve` would give them for events).
//! `fsonos serve` serves clips from its event listener instead.

use fsonos_api::surface::announce::{AnnounceDto, AnnounceRequest, Announcements};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::CoreError;
use fsonos_core::announce::clip::MediaStore;
use fsonos_proto::net::{EventSink, MediaFiles};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use crate::config::GlobalArgs;
use crate::direct::Direct;

/// `fsonos say` options.
#[derive(Debug, Clone, clap::Args)]
pub struct SayArgs {
    /// What to say (macOS `say`; at most 1000 characters).
    pub text: String,
    /// The `say` voice [default: the system voice].
    #[arg(long)]
    pub voice: Option<String>,
    #[command(flatten)]
    pub at: Where,
}

/// `fsonos chime` options.
#[derive(Debug, Clone, clap::Args)]
pub struct ChimeArgs {
    /// The chime: bell, beep or rise.
    pub name: String,
    #[command(flatten)]
    pub at: Where,
}

/// Where and how loud.
#[derive(Debug, Clone, clap::Args)]
pub struct Where {
    /// The rooms (names, aliases or `all`; repeat the flag or separate with
    /// commas) [default: every room].
    #[arg(long = "rooms", alias = "room", value_delimiter = ',')]
    pub rooms: Vec<String>,
    /// The level to announce at, 0-100 [default: 35]; the house policy caps
    /// it per room.
    #[arg(long)]
    pub volume: Option<u8>,
}

impl SayArgs {
    #[must_use]
    pub fn request(&self) -> AnnounceRequest {
        AnnounceRequest {
            text: Some(self.text.clone()),
            voice: self.voice.clone(),
            ..self.at.request()
        }
    }
}

impl ChimeArgs {
    #[must_use]
    pub fn request(&self) -> AnnounceRequest {
        AnnounceRequest {
            chime: Some(self.name.clone()),
            ..self.at.request()
        }
    }
}

impl Where {
    fn request(&self) -> AnnounceRequest {
        AnnounceRequest {
            rooms: self.rooms.clone(),
            volume: self.volume.map(i64::from),
            ..AnnounceRequest::default()
        }
    }
}

/// Clips kept in the data directory (a temporary one when there is none),
/// and how a listener finds them by name.
#[must_use]
pub fn media(data_dir: Option<&Path>) -> (MediaStore, MediaFiles) {
    let store = data_dir.map_or_else(
        || MediaStore::new(&std::env::temp_dir().join("fsonos")),
        MediaStore::new,
    );
    let files = store.clone();
    (store, Arc::new(move |name: &str| files.path(name)))
}

/// Announce `req` in direct mode, serving the clip from a listener of its
/// own until the announcement is over.
pub fn run(global: &GlobalArgs, req: &AnnounceRequest) -> Result<AnnounceDto, Failure> {
    // A bad request fails before the survey.
    req.source()?;
    req.volume()?;
    let direct = Direct::survey(global)?;
    let toward = direct
        .households()?
        .iter()
        .flat_map(|h| &h.players)
        .map(|p| p.ip)
        .next()
        .ok_or_else(|| Failure::new(ErrorCode::NotReady, "no player to serve the clip toward"))?;
    let network = global.network()?;
    let local = network
        .lan
        .local_address_toward(toward)
        .map_err(|e| Failure::from(CoreError::from(e)))?;
    let (store, files) = media(global.data_dir().as_deref());
    let sink = EventSink::start_serving(SocketAddr::new(local, 0), Some(files))
        .map_err(|e| Failure::from(CoreError::from(e)))?;
    let base = sink.callback_url("").trim_end_matches('/').to_string();
    let announcements = Announcements::new(store, Box::new(move || Some(base.clone())));
    let announced = direct.announce(announcements, req);
    drop(sink);
    announced
}

/// One line for people.
#[must_use]
pub fn text(announced: &AnnounceDto) -> String {
    format!("{}\n", announced.done)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_become_the_request() {
        let say = SayArgs {
            text: "dinner".into(),
            voice: Some("Samantha".into()),
            at: Where {
                rooms: vec!["Kitchen".into(), "Office".into()],
                volume: Some(30),
            },
        };
        let req = say.request();
        assert_eq!(
            (
                req.text.as_deref(),
                req.voice.as_deref(),
                req.chime.as_deref()
            ),
            (Some("dinner"), Some("Samantha"), None)
        );
        assert_eq!((req.rooms.len(), req.volume), (2, Some(30)));
        let chime = ChimeArgs {
            name: "bell".into(),
            at: Where {
                rooms: Vec::new(),
                volume: None,
            },
        }
        .request();
        assert_eq!(chime.chime.as_deref(), Some("bell"));
        assert!(chime.rooms.is_empty() && chime.volume.is_none());
    }

    #[test]
    fn media_is_served_only_by_clip_name() {
        let dir = std::env::temp_dir().join(format!("fsonos-cli-media-{}", std::process::id()));
        let (store, files) = media(Some(&dir));
        let clip = store
            .put(&fsonos_core::announce::clip::Chime::Beep.wav())
            .unwrap();
        assert!(files(&format!("{}.wav", clip.id)).is_some());
        assert!(files("../fsonos.db").is_none());
        assert!(files(&clip.id).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
