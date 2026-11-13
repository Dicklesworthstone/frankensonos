//! `fsonos say`, `chime`, and `announce --file`: an announcement from the command line.
//!
//! In direct mode the CLI serves the clip itself, for as long as the
//! announcement takes, from listeners on the addresses it uses to reach the
//! speakers (the addresses `fsonos serve` would give them for events).
//! `fsonos serve` serves clips from its event listener instead.

use fsonos_api::surface::announce::{
    AnnounceDto, AnnounceRequest, AnnouncementJobDto, Announcements,
};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::announce::clip::{MediaStore, read_wav};
use fsonos_proto::net::MediaFiles;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::GlobalArgs;
use crate::direct::Direct;

mod listeners;

/// `fsonos say` options.
#[derive(Debug, Clone, clap::Args)]
pub struct SayArgs {
    /// What to say with the local speech backend (at most 1000 characters).
    pub text: String,
    /// The backend's voice [default: the configured voice].
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

/// `fsonos announce --file` options.
#[derive(Debug, Clone, clap::Args)]
pub struct AnnounceArgs {
    /// A local 16-bit PCM WAV (at most 16 MiB and five minutes).
    /// The CLI uploads its bytes when using the daemon.
    #[arg(long)]
    pub file: PathBuf,
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
    /// Return a daemon job ID after admission. Inspect or cancel it with
    /// `fsonos announcements`; requires a running daemon.
    #[arg(long)]
    pub background: bool,
}

/// Inspect or cancel process-local announcement jobs held by the daemon.
#[derive(Debug, Clone, clap::Args)]
pub struct JobsArgs {
    #[command(subcommand)]
    pub action: Option<JobsCommand>,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum JobsCommand {
    /// List active jobs and recent receipts visible to this caller.
    List,
    /// Inspect one exact announcement job and its restoration report.
    Status { id: String },
    /// Request cancellation; cleanup may still be running in the reply.
    Cancel { id: String },
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

impl AnnounceArgs {
    pub fn request(&self) -> Result<AnnounceRequest, Failure> {
        let bytes = read_wav(&self.file).map_err(|e| {
            Failure::invalid(format!("cannot read announcement WAV: {e}"))
                .with_hint("Choose a readable 16-bit PCM WAV, at most 16 MiB and five minutes.")
        })?;
        Ok(AnnounceRequest {
            rooms: self.at.rooms.clone(),
            volume: self.at.volume.map(i64::from),
            ..AnnounceRequest::from_wav(&bytes)?
        })
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

/// Run a direct announcement and print its result in the requested format.
pub fn run_and_emit(
    global: &GlobalArgs,
    req: &AnnounceRequest,
    background: bool,
) -> anyhow::Result<()> {
    if background {
        let daemon = required_daemon(global)?;
        return through(&daemon, global.json, req, true);
    }
    let announced = run(global, req)?;
    crate::emit(global.json, &announced, text)
}

fn required_daemon(global: &GlobalArgs) -> Result<crate::remote::Daemon, Failure> {
    crate::remote::Daemon::find(global, true)?.ok_or_else(|| {
        Failure::new(ErrorCode::NotReady, "announcement jobs require a running daemon")
            .with_hint("Run fsonos serve and omit --direct. Synchronous announcements remain available directly.")
    })
}

pub fn through(
    daemon: &crate::remote::Daemon,
    json: bool,
    req: &AnnounceRequest,
    background: bool,
) -> anyhow::Result<()> {
    if background {
        let job: AnnouncementJobDto = daemon.post("/announcements", req)?;
        crate::emit(json, &job, AnnouncementJobDto::text)
    } else {
        let announced = daemon.announce(req)?;
        crate::emit(json, &announced, text)
    }
}

pub fn jobs(global: &GlobalArgs, args: &JobsArgs) -> anyhow::Result<()> {
    let daemon = required_daemon(global)?;
    match args.action.as_ref() {
        None | Some(JobsCommand::List) => {
            let jobs: Vec<AnnouncementJobDto> = daemon.get("/announcements")?;
            crate::emit(global.json, &jobs, |jobs| {
                if jobs.is_empty() {
                    "no retained announcements\n".into()
                } else {
                    jobs.iter().map(AnnouncementJobDto::text).collect()
                }
            })
        }
        Some(JobsCommand::Status { id } | JobsCommand::Cancel { id }) => {
            if id.len() != 32
                || !id
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            {
                return Err(Failure::invalid(
                    "use the exact 32-character announcement id returned by the daemon",
                )
                .into());
            }
            let path = format!("/announcements/{id}");
            let job: AnnouncementJobDto =
                if matches!(args.action.as_ref(), Some(JobsCommand::Cancel { .. })) {
                    daemon.post(&format!("{path}/cancel"), &serde_json::json!({}))?
                } else {
                    daemon.get(&path)?
                };
            crate::emit(global.json, &job, AnnouncementJobDto::text)
        }
    }
}

/// Announce `req` in direct mode, serving the clip from a listener of its
/// own until the announcement is over.
pub fn run(global: &GlobalArgs, req: &AnnounceRequest) -> Result<AnnounceDto, Failure> {
    // A bad request fails before the survey.
    req.source()?;
    req.volume()?;
    let direct = Direct::survey(global)?;
    let network = global.network()?;
    let (store, files) = media(global.data_dir().as_deref());
    let listeners = listeners::Listeners::start(direct.households()?, &files, |toward| {
        network.lan.local_address_toward(toward)
    })?;
    let announced = direct.announce(listeners.announcements(store), req);
    drop(listeners);
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
                background: false,
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
                background: false,
            },
        }
        .request();
        assert_eq!(chime.chime.as_deref(), Some("bell"));
        assert!(chime.rooms.is_empty() && chime.volume.is_none());
    }

    #[test]
    fn a_file_argument_uploads_validated_bytes_without_a_daemon_path() {
        use fsonos_core::announce::clip::{Chime, ClipSource};
        let dir = std::env::temp_dir().join(format!("fsonos-cli-wav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let args = AnnounceArgs {
            file: dir.join("local announcement.wav"),
            at: Where {
                rooms: vec!["Kitchen".into()],
                volume: Some(25),
                background: false,
            },
        };
        let bytes = Chime::Beep.wav();
        std::fs::write(&args.file, &bytes).unwrap();
        let request = args.request().unwrap();
        assert_eq!(request.source().unwrap(), ClipSource::Wav { bytes });
        assert_eq!(
            (request.rooms.as_slice(), request.volume),
            (&["Kitchen".to_string()][..], Some(25))
        );
        let wire = serde_json::to_string(&request).unwrap();
        assert!(!wire.contains("local announcement.wav"));
        std::fs::write(&args.file, b"broken WAV").unwrap();
        assert_eq!(args.request().unwrap_err().code, ErrorCode::InvalidArgument);
        let missing = AnnounceArgs {
            file: dir.join("missing.wav"),
            ..args
        };
        assert_eq!(
            missing.request().unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let _ = std::fs::remove_dir_all(dir);
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
