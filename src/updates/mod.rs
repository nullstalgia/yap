use color_eyre::Result;
use crossbeam::channel::{Receiver, Sender, bounded};
use ratatui::style::Color;
use self_update::{get_target, update::ReleaseAsset};
use semver::Version;
use tracing::{error, info};

use crate::app::{App, Event};

#[cfg(feature = "self-replace")]
use {
    fs_err as fs,
    http::HeaderMap,
    sha2::{Digest, Sha512},
    std::env::{consts::EXE_SUFFIX, current_exe},
    std::io::{BufReader, BufWriter, Read, Write},
    std::path::PathBuf,
    std::process::Command,
};

mod tui;
pub use tui::*;

type HandleResult<T> = Result<T, UpdateBackendMissing>;

#[derive(Debug, thiserror::Error)]
#[error("update backend rx handle dropped")]
pub struct UpdateBackendMissing;

impl<T> From<crossbeam::channel::SendError<T>> for UpdateBackendMissing {
    fn from(_: crossbeam::channel::SendError<T>) -> Self {
        Self
    }
}

#[derive(Debug)]
enum UpdateCommand {
    CheckForUpdate {
        allow_pre_releases: bool,
    },
    #[cfg(feature = "self-replace")]
    DownloadUpdate,
    #[cfg(feature = "self-replace")]
    LaunchUpdatedApp,
}

#[derive(Debug)]
pub enum UpdateEvent {
    UpToDate,
    UpdateFound(String),
    UpdateCheckError(UpdateError),
    #[cfg(feature = "self-replace")]
    DownloadProgress(f64),
    #[cfg(feature = "self-replace")]
    ReadyToLaunch,
    #[cfg(feature = "self-replace")]
    UpdateError(UpdateError),
}

impl From<UpdateEvent> for Event {
    fn from(value: UpdateEvent) -> Self {
        Self::Updates(value)
    }
}

#[derive(Debug)]
pub struct UpdateBackend {
    command_rx: Receiver<UpdateCommand>,
    event_tx: Sender<Event>,
    /// Link to archive containing the updated executable
    archive_asset: Option<ReleaseAsset>,
    /// Link to SHA512 checksum for `archive_asset`
    checksum_asset: Option<ReleaseAsset>,
    #[cfg(feature = "self-replace")]
    current_exe: PathBuf,
}
impl UpdateBackend {
    fn new(receiver: Receiver<UpdateCommand>, event_tx: Sender<Event>) -> Self {
        UpdateBackend {
            command_rx: receiver,
            event_tx,
            archive_asset: None,
            checksum_asset: None,
            #[cfg(feature = "self-replace")]
            current_exe: current_exe().expect("failed to get path of executable"),
        }
    }

    fn handle_message(&mut self, msg: UpdateCommand) -> Result<(), UpdateError> {
        match msg {
            UpdateCommand::CheckForUpdate { allow_pre_releases } => {
                match self.check_for_update(allow_pre_releases) {
                    Ok(Some(new)) => self.event_tx.send(UpdateEvent::UpdateFound(new).into())?,
                    Ok(None) => self.event_tx.send(UpdateEvent::UpToDate.into())?,
                    Err(e) => self
                        .event_tx
                        .send(UpdateEvent::UpdateCheckError(e).into())?,
                }
            }
            #[cfg(feature = "self-replace")]
            UpdateCommand::DownloadUpdate => match self.begin_update() {
                Ok(()) => self.event_tx.send(UpdateEvent::ReadyToLaunch.into())?,
                Err(e) => self.event_tx.send(UpdateEvent::UpdateError(e).into())?,
            },
            #[cfg(feature = "self-replace")]
            UpdateCommand::LaunchUpdatedApp => match self.start_new_version() {
                Err(e) => self
                    .event_tx
                    .send(UpdateEvent::UpdateError(UpdateError::StartNewVersion(e)).into())?,
                _ => unreachable!("should never return Ok(())"),
            },
        }
        Ok(())
    }

    /// Returns Some(String) with the newest released version if found.
    ///
    /// Returns None if the current app version is the newest/newer.
    fn check_for_update(
        &mut self,
        allow_pre_releases: bool,
    ) -> Result<Option<String>, UpdateError> {
        #[cfg(feature = "yap-full")]
        let bin_flavor = "yap-full";
        #[cfg(all(feature = "yap-lite", not(feature = "yap-full")))]
        let bin_flavor = "yap-lite";
        // Used when self-replacing isn't enabled (such as when no flavor was set)
        // so it just checks for anything newer, regardless of flavor.
        #[cfg(not(any(feature = "yap-full", feature = "yap-lite")))]
        let bin_flavor = "yap";

        let current_str = env!("CARGO_PKG_VERSION");
        let current = Version::parse(current_str).expect("failed to parse app's own semver");
        let mut update_builder = self_update::backends::github::Update::configure();
        update_builder
            .repo_owner("nullstalgia")
            .repo_name("yap")
            .bin_name("yap")
            .current_version(current_str);

        let auth_token = option_env!("GITHUB_AUTH_TOKEN");
        if let Some(auth_token) = auth_token {
            update_builder.auth_token(auth_token);
        }

        let releases = update_builder.build()?.get_latest_releases(current_str)?;

        let newest = releases
            .into_iter()
            .filter(|rel| allow_pre_releases || !rel.version.contains("pre"))
            .filter_map(|rel| match Version::parse(&rel.version) {
                Ok(v) => Some((rel, v)),
                Err(e) => {
                    error!("Failed to parse semver from {}, {e}", rel.version);
                    None
                }
            })
            .filter(|(_, ver)| *ver > current)
            .max_by(|(_, a_ver), (_, b_ver)| a_ver.cmp(b_ver));

        let Some((release, _version)) = newest else {
            return Ok(None);
        };

        let target = get_target();
        let Some((archive, checksum)) = asset_pair_for(bin_flavor, target, &release.assets) else {
            error!("Couldn't find SHA+Archive for {bin_flavor} on {target}");
            return Err(UpdateError::ChecksumOrFlavorMissing)?;
        };

        info!(
            "Update found! v{} archive name: {}, checksum name: {}",
            release.version, archive.name, checksum.name
        );

        self.archive_asset = Some(archive.clone());
        self.checksum_asset = Some(checksum.clone());

        Ok(Some(release.version))
    }
    #[cfg(feature = "self-replace")]
    /// Streams the supplied URL's contents into the given File, checking the SHA512 hash of the archive with a supplied checksum by URL.
    fn download_and_verify<T: Write + Unpin>(
        &self,
        archive_url: String,
        checksum_url: String,
        mut file: T,
    ) -> Result<(), UpdateError> {
        let mut headers = HeaderMap::default();
        headers.insert(
            http::header::ACCEPT,
            "application/octet-stream"
                .parse()
                .expect("pre-validated header error?"),
        );
        headers.insert(
            http::header::USER_AGENT,
            "yap/self-update".parse().expect("invalid user-agent"),
        );

        let auth_token = option_env!("GITHUB_AUTH_TOKEN");
        if let Some(auth_token) = auth_token {
            headers.insert(
                http::header::AUTHORIZATION,
                (String::from("token ") + auth_token).parse().unwrap(),
            );
        }

        let client = reqwest::blocking::ClientBuilder::new()
            .default_headers(headers)
            .build()?;

        let resp = client.get(&checksum_url).send()?;
        let size = resp.content_length().unwrap_or(0);
        if !resp.status().is_success() || size == 0 {
            error!("Failed to get archive checksum!");
            return Err(UpdateError::InvalidHttpCode(resp.status().as_u16()));
        }

        let content = resp.text()?;
        // Format is `checksum *filename`
        // So we just want the first "word" in the line
        let expected = content
            .split_whitespace()
            .next()
            .ok_or(UpdateError::ChecksumEmpty)?;

        let resp = client.get(&archive_url).send()?;
        let size = resp.content_length().unwrap_or(0);
        if !resp.status().is_success() || size == 0 {
            error!("Failed to get archive!");
            return Err(UpdateError::InvalidHttpCode(resp.status().as_u16()));
        }

        let mut downloaded: u64 = 0;
        let mut hasher = Sha512::new();
        let mut reader = BufReader::new(resp);

        let mut buffer = [0; 1024 * 8];
        loop {
            match reader.read(&mut buffer) {
                Ok(n) => {
                    if n == 0 {
                        break;
                    }
                    hasher.update(&buffer[..n]);
                    file.write_all(&buffer[..n]).map_err(UpdateError::Temp)?;
                    downloaded += n as u64;
                    let percentage = downloaded as f64 / size as f64;
                    self.event_tx
                        .send(UpdateEvent::DownloadProgress(percentage).into())?;
                }
                Err(e) => return Err(UpdateError::Download(e)),
            }
        }

        let result = hasher.finalize();
        let checksum = format!("{result:x}");

        if checksum.eq(expected) {
            info!("Update checksum matches expected! SHA512: {expected}");
            Ok(())
        } else {
            error!(
                "Archive SHA512 checksum mismatch! Expected: {expected} != Calculated: {checksum}"
            );
            Err(UpdateError::ChecksumMismatch {
                expected: expected.to_owned(),
                got: checksum,
            })
        }
    }
    #[cfg(feature = "self-replace")]
    /// Begin the process of downloading and verifying the archive,
    /// extracting the new binary, and replacing the currently-running executable.
    fn begin_update(&mut self) -> Result<(), UpdateError> {
        let archive = self.archive_asset.take().expect("Missing archive asset");
        let checksum = self.checksum_asset.take().expect("Missing checksum asset");

        // A lot yoinked from
        // https://github.com/jaemk/self_update/blob/60b3c13533e731650031ee2c410f4bbb4483e845/src/update.rs#L227
        let tmp_archive_dir = self_update::TempDir::new().map_err(UpdateError::Temp)?;
        let tmp_archive_path = tmp_archive_dir.path().join(&archive.name);
        let tmp_archive = fs::File::create(&tmp_archive_path).map_err(UpdateError::Temp)?;
        let mut archive_writer = BufWriter::new(tmp_archive);

        info!("Temp archive location: {}", tmp_archive_path.display());

        self.download_and_verify(
            archive.download_url,
            checksum.download_url,
            &mut archive_writer,
        )?;

        archive_writer.flush().map_err(UpdateError::Temp)?;

        let bin_name = env!("CARGO_PKG_NAME");
        let bin_name = format!("{bin_name}{EXE_SUFFIX}");

        self_update::Extract::from_source(&tmp_archive_path)
            .extract_file(tmp_archive_dir.path(), &bin_name)?;

        let new_exe = tmp_archive_dir.path().join(bin_name);

        self_replace::self_replace(new_exe).map_err(UpdateError::SelfReplace)?;

        Ok(())
    }
    #[cfg(feature = "self-replace")]
    /// This should never return, unless an error occurs.
    fn start_new_version(&mut self) -> Result<(), std::io::Error> {
        let current_exe = self.current_exe.clone();

        // In the happy path, this function call won't return back to us
        // since we're ending the process and replacing it with the new one
        let error = restart_process(current_exe);
        Err(error)?
    }
    fn work_loop(&mut self) {
        while let Ok(msg) = self.command_rx.recv() {
            if let Err(e) = self.handle_message(msg) {
                error!("update worker had an unexpected error: {e}");
                break;
            }
        }
        info!("update worker has shut down.")
    }
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("failed to send event to main app")]
    EventSend,
    #[error("handle dropped, can't recieve commands")]
    HandleDropped,
    #[error("error getting update information")]
    SelfUpdate(#[from] self_update::errors::Error),
    #[error("reqwest web error")]
    Reqwest(#[from] reqwest::Error),
    #[error("error getting response contents")]
    Download(#[source] std::io::Error),
    #[error("unexpected http status: {0}")]
    InvalidHttpCode(u16),
    #[error("error with temporary folder/file")]
    Temp(#[source] std::io::Error),
    #[error("SHA512 checksum of downloaded file does not match expected: {expected} != {got}")]
    ChecksumMismatch { expected: String, got: String },
    #[error("release assets for flavor were not found")]
    ChecksumOrFlavorMissing,
    #[error("checksum file was missing expected contents")]
    ChecksumEmpty,
    #[error("failed to replace current executable")]
    SelfReplace(#[source] std::io::Error),
    #[error("failed to start newly downloaded executable")]
    StartNewVersion(#[source] std::io::Error),
}

impl<T> From<crossbeam::channel::SendError<T>> for UpdateError {
    fn from(_: crossbeam::channel::SendError<T>) -> Self {
        Self::EventSend
    }
}

/// Returns a pair of ReleaseAssets for the given target from the list of assets
///
/// Returns None if there aren't exactly two files for the given target and flavor
/// (either there's too many or too little, we expect one checksum per archive).
///
/// Returns Assets in the order of (Archive, SHA512 Checksum)
fn asset_pair_for<'a>(
    flavor: &str,
    target: &str,
    releases: &'a [ReleaseAsset],
) -> Option<(&'a ReleaseAsset, &'a ReleaseAsset)> {
    let assets: Vec<&ReleaseAsset> = releases
        .iter()
        .filter(|asset| asset.name.contains(target))
        .filter(|asset| asset.name.contains(flavor))
        .collect();

    #[cfg(any(feature = "yap-full", feature = "yap-lite"))]
    // If we're checking for just `yap`, we're gonna have more than one possible pair
    // to pick from, so don't include this check if we aren't looking for
    // a specific flavor.
    if assets.len() != 2 {
        return None;
    }

    // I'm gonna assume we get the pair in a non-determinate order, so let's sort them ourselves.
    let (checksums, archives): (Vec<&ReleaseAsset>, Vec<&ReleaseAsset>) = assets
        .iter()
        .partition(|asset| asset.name.ends_with(".sha512"));

    // Should be symmetrical since they should come in pairs
    if checksums.len() != archives.len() {
        return None;
    }

    Some((archives[0], checksums[0]))
}

#[derive(Debug)]
pub struct UpdateHandle {
    command_tx: Sender<UpdateCommand>,
}

impl UpdateHandle {
    pub fn new(event_tx: Sender<Event>) -> Self {
        let (command_tx, command_rx) = bounded(5);
        let mut actor = UpdateBackend::new(command_rx, event_tx);
        let _join_handle = std::thread::spawn(move || {
            actor.work_loop();
        });
        Self { command_tx }
    }
    pub fn query_latest(&self, allow_pre_releases: bool) -> HandleResult<()> {
        self.command_tx
            .send(UpdateCommand::CheckForUpdate { allow_pre_releases })?;
        Ok(())
    }
    #[cfg(feature = "self-replace")]
    pub fn download_update(&self) -> HandleResult<()> {
        self.command_tx.send(UpdateCommand::DownloadUpdate)?;
        Ok(())
    }
    #[cfg(feature = "self-replace")]
    pub fn start_new_version(&self) -> HandleResult<()> {
        self.command_tx.send(UpdateCommand::LaunchUpdatedApp)?;
        Ok(())
    }
}

// Yoinked from
// https://github.com/lichess-org/fishnet/blob/eac238abbd77b7fc8cacd2d1f7c408252746e2f5/src/main.rs#L399

#[cfg(feature = "self-replace")]
fn restart_process(current_exe: PathBuf) -> std::io::Error {
    exec(Command::new(current_exe).args(std::env::args_os().skip(1)))
}

#[cfg(unix)]
#[cfg(feature = "self-replace")]
fn exec(command: &mut Command) -> std::io::Error {
    use std::os::unix::process::CommandExt as _;
    // Completely replace the current process image. If successful, execution
    // of the current process stops here.
    command.exec()
}

#[cfg(windows)]
#[cfg(feature = "self-replace")]
fn exec(command: &mut Command) -> std::io::Error {
    use std::os::windows::process::CommandExt as _;
    // No equivalent for Unix exec() exists. So create a new independent
    // console instead and terminate the current one:
    // https://docs.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
    let create_new_console = 0x0000_0010;
    match command.creation_flags(create_new_console).spawn() {
        Ok(_) => std::process::exit(libc::EXIT_SUCCESS),
        Err(err) => err,
    }
}

impl App {
    pub fn update_begin_choice(&mut self, choice: UpdateBeginPrompt) -> Result<()> {
        match choice {
            #[cfg(feature = "self-replace")]
            UpdateBeginPrompt::DownloadAndInstall => {
                use crate::app::Popup;

                self.update_worker.download_update()?;
                // I normally dont set popups like this,
                // but I didn't want to have the event carousel thread events bouncing
                self.popup = Some(Popup::UpdateDownloading(0.0));
            }
            UpdateBeginPrompt::OpenGithubRepo => {
                let url = format!("{}/releases", env!("CARGO_PKG_REPOSITORY"));
                info!("Opening {url} in browser");
                if let Err(e) = opener::open_browser(url) {
                    let err = format!("Failed to open app repository! {e}");
                    self.notifs.notify_str(err, Color::Red);
                }
            }
            UpdateBeginPrompt::AskAgainLater => self.dismiss_popup(),
            UpdateBeginPrompt::SkipVersion => {
                self.settings.updates.skipped_version = self
                    .update_found_version
                    .take()
                    .expect("prompt shouldnt appear without this being filled");
                self.settings.save()?;
                self.dismiss_popup();
            }
        }
        Ok(())
    }
    pub fn update_check_consent_choice(&mut self, choice: UpdateCheckConsentPrompt) -> Result<()> {
        match choice {
            UpdateCheckConsentPrompt::Yes => {
                self.settings.updates.allow_checking_for_updates = true;
                self.settings.updates.user_dismissed_prompt = true;
                self.settings.save()?;
                if self
                    .update_worker
                    .query_latest(self.settings.updates.allow_pre_releases)
                    .is_err()
                {
                    error!("Update backend missing! Did a previous check attempt fail?")
                };
            }
            UpdateCheckConsentPrompt::Never => {
                self.settings.updates.allow_checking_for_updates = false;
                self.settings.updates.user_dismissed_prompt = true;
                self.settings.save()?;
            }
            UpdateCheckConsentPrompt::AskAgainLater => (),
        }
        self.dismiss_popup();
        Ok(())
    }
    #[cfg(all(windows, feature = "self-replace"))]
    pub fn update_launch_choice(&mut self, choice: UpdateLaunchPrompt) -> Result<()> {
        match choice {
            UpdateLaunchPrompt::OpenInNewWindow => {
                self.update_worker.start_new_version()?;
            }
            UpdateLaunchPrompt::Close => self.shutdown(),
        }

        Ok(())
    }
}
