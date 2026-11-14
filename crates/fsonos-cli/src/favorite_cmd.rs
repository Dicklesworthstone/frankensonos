//! My Sonos catalog edits use the selected daemon once, with no local replay.

use clap::Args;
use fsonos_api::Failure;
use fsonos_api::surface::favorite_management::{
    self, InspectFavoriteRequest, ManagedFavoriteDto, ObservedFavoriteDto, RemoveFavoriteRequest,
    RenameFavoriteRequest, SaveFavoriteRequest,
};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct FavoriteInspectArgs {
    /// A room in the favorite's household; Room@S1 / Room@S2 disambiguates names.
    pub zone: String,
    /// Exact FV:2/<n> id from favorites; titles and catalog numbers are not accepted.
    pub favorite: String,
}

#[derive(Debug, Args)]
pub struct FavoriteSaveArgs {
    /// Room whose selected native track or station will become a favorite.
    pub zone: String,
    /// Favorite name (nonblank, no controls, at most 1023 UTF-8 bytes).
    #[arg(value_parser = title)]
    pub title: String,
    /// Exact revision from favorite-current; a changed source is refused.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

#[derive(Debug, Args)]
pub struct FavoriteRenameArgs {
    pub zone: String,
    /// Exact FV:2/<n> id from favorite-inspect.
    pub favorite: String,
    #[arg(value_parser = title)]
    pub title: String,
    /// Exact revision from favorite-inspect in this household/group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

#[derive(Debug, Args)]
pub struct FavoriteRemoveArgs {
    pub zone: String,
    /// Exact FV:2/<n> id to remove from My Sonos. Playback continues.
    pub favorite: String,
    /// Exact revision from favorite-inspect in this household/group.
    #[arg(long, value_name = "REVISION")]
    pub expected_revision: String,
}

fn title(value: &str) -> Result<String, String> {
    fsonos_core::favorites::validate_title(value)
        .map(str::to_owned)
        .map_err(str::to_owned)
}

pub fn current(global: &GlobalArgs, zone: &str) -> anyhow::Result<()> {
    let path = favorite_management::current_path(zone)?;
    let result: ObservedFavoriteDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.get(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        daemon::surface(global, daemon::policy(&dir)?)?.current_favorite(&Client::Cli, zone)?
    };
    emit(global.json, &result, ObservedFavoriteDto::text)
}

pub fn inspect(global: &GlobalArgs, args: &FavoriteInspectArgs) -> anyhow::Result<()> {
    let request = InspectFavoriteRequest {
        zone: args.zone.clone(),
        favorite: args.favorite.clone(),
    };
    let path = request.path()?;
    let result: ObservedFavoriteDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.get(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        daemon::surface(global, daemon::policy(&dir)?)?.inspect_favorite(&Client::Cli, &request)?
    };
    emit(global.json, &result, ObservedFavoriteDto::text)
}

impl FavoriteSaveArgs {
    fn request(&self) -> Result<SaveFavoriteRequest, Failure> {
        let request = SaveFavoriteRequest {
            zone: self.zone.clone(),
            title: self.title.clone(),
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

impl FavoriteRenameArgs {
    fn request(&self) -> Result<RenameFavoriteRequest, Failure> {
        let request = RenameFavoriteRequest {
            zone: self.zone.clone(),
            favorite: self.favorite.clone(),
            title: self.title.clone(),
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

impl FavoriteRemoveArgs {
    fn request(&self) -> Result<RemoveFavoriteRequest, Failure> {
        let request = RemoveFavoriteRequest {
            zone: self.zone.clone(),
            favorite: self.favorite.clone(),
            expected_revision: self.expected_revision.clone(),
        };
        request.validate()?;
        Ok(request)
    }
}

pub fn save(global: &GlobalArgs, args: &FavoriteSaveArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: ManagedFavoriteDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/favorites/save", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli").save_favorite(&Client::Cli, &request)?
    };
    emit(global.json, &result, ManagedFavoriteDto::text)
}

pub fn rename(global: &GlobalArgs, args: &FavoriteRenameArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: ManagedFavoriteDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/favorites/rename", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli").rename_favorite(&Client::Cli, &request)?
    };
    emit(global.json, &result, ManagedFavoriteDto::text)
}

pub fn remove(global: &GlobalArgs, args: &FavoriteRemoveArgs) -> anyhow::Result<()> {
    let request = args.request()?;
    let result: ManagedFavoriteDto = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.post("/favorites/remove", &request)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        daemon::with_action_log(surface, &dir, "cli").remove_favorite(&Client::Cli, &request)?
    };
    emit(global.json, &result, ManagedFavoriteDto::text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command};
    use clap::Parser;

    #[test]
    fn favorite_commands_keep_native_identity_revision_and_selected_daemon() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "favorite-save",
            "Den@S2",
            "  Café & friends  ",
            "--expected-revision",
            "source:a&b=2",
            "--daemon",
            "--json",
        ])
        .unwrap();
        assert!(cli.global.daemon && cli.global.json);
        let Command::FavoriteSave(args) = cli.command else {
            panic!("save")
        };
        assert_eq!(
            args.request().unwrap(),
            SaveFavoriteRequest {
                zone: "Den@S2".into(),
                title: "Café & friends".into(),
                expected_revision: "source:a&b=2".into()
            }
        );
        let cli = Cli::try_parse_from([
            "fsonos",
            "favorite-rename",
            "Den@S1",
            "FV:2/7",
            "Evening",
            "--expected-revision",
            "r",
            "--direct",
        ])
        .unwrap();
        assert!(cli.global.direct);
        let Command::FavoriteRename(args) = cli.command else {
            panic!("rename")
        };
        assert_eq!(args.request().unwrap().favorite, "FV:2/7");
        assert!(Cli::try_parse_from(["fsonos", "favorite-remove", "Den", "FV:2/7"]).is_err());
        assert!(Cli::try_parse_from(["fsonos", "favorite-save", "Den", "Song"]).is_err());
    }

    #[test]
    fn invalid_favorite_requests_fail_before_daemon_or_speaker_access() {
        for id in ["Dinner", "7", "FV:2/7/1"] {
            assert!(
                FavoriteRemoveArgs {
                    zone: "Den".into(),
                    favorite: id.into(),
                    expected_revision: "r".into()
                }
                .request()
                .is_err()
            );
        }
        for name in [String::new(), "a\nb".into(), "é".repeat(512)] {
            assert!(title(&name).is_err());
        }
        assert!(
            FavoriteSaveArgs {
                zone: "Den".into(),
                title: "Song".into(),
                expected_revision: " ".into()
            }
            .request()
            .is_err()
        );
    }
}
