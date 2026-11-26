//! Manual GitHub Releases self-updater for TackleCast.
//!
//! Queries GitHub's stable `/releases/latest` endpoint only when explicitly
//! requested by the user. In-place installation requires a release signed
//! by the build's pinned key and a recoverable filesystem transaction.

use std::fmt::{Display, Formatter};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use ring::signature::{UnparsedPublicKey, ED25519};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tempfile::TempDir;

mod archive;
mod install;

/// Default upstream GitHub repository queried for official TackleCast releases.
pub const DEFAULT_UPDATE_REPO: &str = "SaltedByte/TackleCast";

/// Network timeout applied to GitHub API and checksum requests.
const API_TIMEOUT: Duration = Duration::from_secs(15);

const MAX_ARCHIVE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_ASSET_REDIRECTS: u32 = 5;

/// Returns the `owner/repo` slug queried for updates.
///
/// Defaults to [`DEFAULT_UPDATE_REPO`] and can be overridden at compile time
/// via the `TACKLECAST_UPDATE_REPO` environment variable for forks or testing.
pub fn update_repo() -> &'static str {
    option_env!("TACKLECAST_UPDATE_REPO")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(DEFAULT_UPDATE_REPO)
}

/// Returns the running binary's package version from `Cargo.toml`.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Strict `MAJOR.MINOR.PATCH` semantic version used for stable release comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SemVer {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
}

impl SemVer {
    /// Parses a strict stable release tag (`vMAJOR.MINOR.PATCH` or `MAJOR.MINOR.PATCH`).
    ///
    /// Pre-release suffixes (e.g. `-rc.1`) and non-semver tags return `None`.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let body = trimmed.strip_prefix('v').unwrap_or(trimmed);
        let mut parts = body.split('.');
        let major = parse_digits(parts.next()?)?;
        let minor = parse_digits(parts.next()?)?;
        let patch = parse_digits(parts.next()?)?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl Display for SemVer {
    /// Formats the version as `MAJOR.MINOR.PATCH`.
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Parses an ASCII digit-only version component.
fn parse_digits(part: &str) -> Option<u64> {
    if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    part.parse::<u64>().ok()
}

/// Archive container format used by a platform release asset.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    TarGz,
}

/// Expected GitHub Release asset filenames for the current target platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlatformAssetSpec {
    archive_name: &'static str,
    checksum_name: &'static str,
    kind: ArchiveKind,
}

/// Returns the expected official release asset specification for this OS/arch.
fn current_platform_asset() -> Option<PlatformAssetSpec> {
    #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
    {
        Some(PlatformAssetSpec {
            archive_name: "TackleCast-Windows-x86_64.zip",
            checksum_name: "TackleCast-Windows-x86_64.zip.sha256",
            kind: ArchiveKind::Zip,
        })
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Some(PlatformAssetSpec {
            archive_name: "TackleCast-Linux-x86_64.tar.gz",
            checksum_name: "TackleCast-Linux-x86_64.tar.gz.sha256",
            kind: ArchiveKind::TarGz,
        })
    }
    #[cfg(not(any(
        all(target_os = "windows", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "x86_64")
    )))]
    {
        None
    }
}

/// Whether the running executable can be updated in place.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InstallMode {
    PortableWritable(PathBuf),
    ExternalOnly(String),
}

/// Detects whether `exe_dir` is a writable portable TackleCast installation.
///
/// Requires `build-info.json` next to the executable (to avoid overwriting
/// `cargo` dev builds) and verifies write permission with a temporary probe.
fn detect_install_mode(exe_dir: &Path) -> InstallMode {
    if install::read_version(exe_dir).ok() != SemVer::parse(current_version()) {
        return InstallMode::ExternalOnly(
            "Portable release required for in-app update (dev/source build detected).".into(),
        );
    }
    match tempfile::tempfile_in(exe_dir) {
        Ok(_) => InstallMode::PortableWritable(exe_dir.to_path_buf()),
        Err(_) => InstallMode::ExternalOnly(
            "Install folder is read-only; download the release manually.".into(),
        ),
    }
}

/// Resolves the directory containing the currently running executable.
fn current_exe_dir() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("Cannot locate executable: {e}"))?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "Executable has no parent directory.".into())
}

/// UI-facing snapshot of the updater's current state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum UpdateStatus {
    /// No check has been run or the user dismissed the banner.
    #[default]
    Idle,
    /// Querying GitHub's latest release API.
    Checking,
    /// Running version is already equal to or newer than the latest release.
    UpToDate { current: String },
    /// A newer stable release was found.
    Available {
        version: String,
        html_url: String,
        can_self_update: bool,
        fallback_reason: Option<String>,
    },
    /// Downloading the release archive in the background.
    Downloading {
        version: String,
        downloaded: u64,
        total: Option<u64>,
    },
    /// Verifying the downloaded archive against its `.sha256` checksum.
    Verifying { version: String },
    /// Extracting the verified archive into the staging directory.
    Extracting { version: String },
    /// Staged and verified; waiting for the user to click "Restart to apply".
    ReadyToRestart { version: String },
    /// Check, download, verification, or staging failed.
    Failed { message: String },
}

impl UpdateStatus {
    /// Returns `true` while a network or staging operation is in progress.
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Checking
                | Self::Downloading { .. }
                | Self::Verifying { .. }
                | Self::Extracting { .. }
        )
    }
}

/// User actions emitted by the settings UI footer/banner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAction {
    /// Query GitHub for the latest stable release.
    Check,
    /// Start downloading and staging the available update.
    Download,
    /// Cancel an in-progress download.
    Cancel,
    /// Spawn the staged updater helper and exit TackleCast to apply the update.
    RestartToApply,
    /// Open the GitHub release page in the default browser.
    OpenReleasePage(String),
    /// Dismiss the inline update status banner.
    Dismiss,
}

/// Metadata retained for a discovered update that can be downloaded.
#[derive(Debug)]
struct PendingRelease {
    version: String,
    html_url: String,
    download_plan: Option<Box<DownloadPlan>>,
    fallback_reason: Option<String>,
}

/// Concrete download and staging plan for a self-updatable release.
#[derive(Debug, Clone)]
struct DownloadPlan {
    archive_name: String,
    archive_url: String,
    checksum_url: String,
    signature_url: String,
    public_key: [u8; 32],
    repo: String,
    tag: String,
    expected_size: Option<u64>,
    kind: ArchiveKind,
    install_dir: PathBuf,
}

/// Staged update ready to be applied on restart.
struct StagedUpdate {
    version: String,
    work: TempDir,
}

/// Internal messages sent from background worker threads to [`UpdateController`].
enum WorkerMessage {
    CheckComplete(Result<CheckOutcome, String>),
    DownloadProgress {
        version: String,
        downloaded: u64,
        total: Option<u64>,
    },
    Verifying {
        version: String,
    },
    Extracting {
        version: String,
    },
    Staged(StagedUpdate),
    Cancelled,
    Failed(String),
}

/// Result of a completed GitHub release check.
enum CheckOutcome {
    UpToDate { current: String },
    Available(PendingRelease),
}

/// Coordinates background update checks, downloads, staging, and relaunch handoff.
pub struct UpdateController {
    status: UpdateStatus,
    pending: Option<PendingRelease>,
    staged: Option<StagedUpdate>,
    receiver: Option<Receiver<WorkerMessage>>,
    cancel_flag: Arc<AtomicBool>,
    waker: Arc<dyn Fn() + Send + Sync>,
}

impl UpdateController {
    /// Creates a new idle updater controller with a callback that wakes the UI loop.
    pub fn new(waker: impl Fn() + Send + Sync + 'static) -> Self {
        Self {
            status: UpdateStatus::Idle,
            pending: None,
            staged: None,
            receiver: None,
            cancel_flag: Arc::new(AtomicBool::new(false)),
            waker: Arc::new(waker),
        }
    }

    /// Returns the current UI-facing update status snapshot.
    pub fn snapshot(&self) -> &UpdateStatus {
        &self.status
    }

    /// Drains any pending worker messages and updates the status snapshot.
    ///
    /// Returns `true` if the status changed and the UI should redraw.
    pub fn poll(&mut self) -> bool {
        let mut changed = false;
        let mut clear_receiver = false;

        if let Some(receiver) = &self.receiver {
            loop {
                match receiver.try_recv() {
                    Ok(msg) => {
                        changed = true;
                        match msg {
                            WorkerMessage::CheckComplete(Ok(CheckOutcome::UpToDate {
                                current,
                            })) => {
                                self.pending = None;
                                self.staged = None;
                                self.status = UpdateStatus::UpToDate { current };
                                clear_receiver = true;
                            }
                            WorkerMessage::CheckComplete(Ok(CheckOutcome::Available(release))) => {
                                let status = UpdateStatus::Available {
                                    version: release.version.clone(),
                                    html_url: release.html_url.clone(),
                                    can_self_update: release.download_plan.is_some(),
                                    fallback_reason: release.fallback_reason.clone(),
                                };
                                self.pending = Some(release);
                                self.staged = None;
                                self.status = status;
                                clear_receiver = true;
                            }
                            WorkerMessage::CheckComplete(Err(message))
                            | WorkerMessage::Failed(message) => {
                                self.status = UpdateStatus::Failed { message };
                                clear_receiver = true;
                            }
                            WorkerMessage::DownloadProgress {
                                version,
                                downloaded,
                                total,
                            } => {
                                self.status = UpdateStatus::Downloading {
                                    version,
                                    downloaded,
                                    total,
                                };
                            }
                            WorkerMessage::Verifying { version } => {
                                self.status = UpdateStatus::Verifying { version };
                            }
                            WorkerMessage::Extracting { version } => {
                                self.status = UpdateStatus::Extracting { version };
                            }
                            WorkerMessage::Staged(staged) => {
                                self.status = UpdateStatus::ReadyToRestart {
                                    version: staged.version.clone(),
                                };
                                self.staged = Some(staged);
                                clear_receiver = true;
                            }
                            WorkerMessage::Cancelled => {
                                if let Some(release) = &self.pending {
                                    self.status = UpdateStatus::Available {
                                        version: release.version.clone(),
                                        html_url: release.html_url.clone(),
                                        can_self_update: release.download_plan.is_some(),
                                        fallback_reason: release.fallback_reason.clone(),
                                    };
                                } else {
                                    self.status = UpdateStatus::Idle;
                                }
                                clear_receiver = true;
                            }
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        if self.status.is_busy() {
                            self.status = UpdateStatus::Failed {
                                message: "Update worker stopped unexpectedly.".into(),
                            };
                            changed = true;
                        }
                        clear_receiver = true;
                        break;
                    }
                }
            }
        }

        if clear_receiver {
            self.receiver = None;
        }
        changed
    }

    /// Starts a manual background check against GitHub's `/releases/latest` endpoint.
    pub fn check_for_updates(&mut self) {
        if self.status.is_busy() {
            return;
        }
        self.status = UpdateStatus::Checking;
        self.staged = None;
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        let waker = Arc::clone(&self.waker);
        let repo = update_repo().to_string();
        let current = current_version().to_string();

        std::thread::spawn(move || {
            let result = fetch_latest_release(&repo, &current);
            let _ = tx.send(WorkerMessage::CheckComplete(result));
            (waker)();
        });
    }

    /// Starts downloading, verifying, and staging the discovered update.
    pub fn start_download(&mut self) {
        if self.status.is_busy() {
            return;
        }
        let Some(release) = &self.pending else {
            return;
        };
        let Some(plan) = release.download_plan.clone() else {
            return;
        };
        let version = release.version.clone();

        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel_flag);
        let waker = Arc::clone(&self.waker);
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.status = UpdateStatus::Downloading {
            version: version.clone(),
            downloaded: 0,
            total: plan.expected_size,
        };

        std::thread::spawn(move || {
            download_and_stage(version, *plan, cancel, &tx, &waker);
        });
    }

    /// Signals an active download worker to stop and clean up partial files.
    pub fn cancel_download(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Dismisses the status banner when no background task is actively running.
    pub fn dismiss(&mut self) {
        if !self.status.is_busy() {
            self.pending = None;
            self.staged = None;
            self.status = UpdateStatus::Idle;
        }
    }

    /// Spawns the staged binary in `--apply-update` mode so the caller can exit.
    pub fn launch_staged_updater(&mut self) -> Result<(), String> {
        let Some(staged) = self.staged.take() else {
            return Err("No staged update is ready to apply.".into());
        };
        if let Err(err) = install::spawn_helper(staged.work) {
            let err = format!("Could not launch the staged update: {err}");
            self.status = UpdateStatus::Failed {
                message: err.clone(),
            };
            return Err(err);
        }
        Ok(())
    }
}

/// Minimal GitHub Release JSON payload returned by `/releases/latest`.
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

/// Minimal GitHub Release asset entry.
#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

/// Maximum response body size accepted for GitHub API release metadata (1 MiB).
const MAX_API_RESPONSE_BYTES: u64 = 1024 * 1024;

/// Maximum response body size accepted for a `.sha256` checksum asset (64 KiB).
const MAX_CHECKSUM_BYTES: u64 = 64 * 1024;

fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .max_redirects(0)
        .timeout_connect(Some(API_TIMEOUT))
        .timeout_per_read(Some(API_TIMEOUT))
        .timeout_recv_response(Some(API_TIMEOUT))
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn read_bounded<R: Read>(reader: R, max_bytes: u64, label: &str) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Failed reading {label}: {e}"))?;
    if buf.len() as u64 > max_bytes {
        return Err(format!("{label} exceeded maximum allowed size."));
    }
    Ok(buf)
}

fn read_bounded_utf8(reader: impl Read, max_bytes: u64, label: &str) -> Result<String, String> {
    String::from_utf8(read_bounded(reader, max_bytes, label)?)
        .map_err(|err| format!("Invalid UTF-8 in {label}: {err}"))
}

fn validate_repo(repo: &str) -> Result<(), String> {
    let valid_part = |part: &str| {
        !matches!(part, "" | "." | "..")
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    };
    if repo
        .split_once('/')
        .is_some_and(|(owner, name)| valid_part(owner) && valid_part(name))
    {
        Ok(())
    } else {
        Err("Invalid update repository. Expected owner/repo.".into())
    }
}

fn validate_https_download_url(url: &str) -> Result<ureq::http::Uri, String> {
    let uri: ureq::http::Uri = url.parse().map_err(|_| "Invalid update asset URL.")?;
    let authority = uri
        .authority()
        .map(|authority| authority.as_str().to_ascii_lowercase());
    if uri.scheme_str() != Some("https")
        || !matches!(
            authority.as_deref(),
            Some(
                "github.com"
                    | "release-assets.githubusercontent.com"
                    | "objects.githubusercontent.com"
            )
        )
        || url.contains(['#', '\\'])
    {
        return Err("Update asset URL must use HTTPS on GitHub's release hosts.".into());
    }
    Ok(uri)
}

fn validate_release_asset(asset: &GitHubAsset, repo: &str, tag: &str) -> Result<(), String> {
    let uri = validate_https_download_url(&asset.browser_download_url)?;
    let path = uri.path();
    let prefix = format!("/{repo}/releases/download/");
    let suffix = format!("{tag}/{}", asset.name);
    if !uri
        .host()
        .is_some_and(|host| host.eq_ignore_ascii_case("github.com"))
        || uri.query().is_some()
        || !path
            .get(..prefix.len())
            .is_some_and(|part| part.eq_ignore_ascii_case(&prefix))
        || path.get(prefix.len()..) != Some(suffix.as_str())
    {
        return Err("Update asset URL does not match the selected repository and release.".into());
    }
    Ok(())
}

fn open_asset(
    agent: &ureq::Agent,
    url: &str,
    user_agent: &str,
) -> Result<ureq::http::Response<ureq::Body>, String> {
    let mut uri = validate_https_download_url(url)?;
    for _ in 0..=MAX_ASSET_REDIRECTS {
        let response = agent
            .get(&uri)
            .header("User-Agent", user_agent)
            .call()
            .map_err(format_http_error)?;
        if response.status() == ureq::http::StatusCode::OK {
            return Ok(response);
        }
        if !response.status().is_redirection() {
            return Err(format!(
                "Unexpected update download status: {}",
                response.status()
            ));
        }
        let location = response
            .headers()
            .get("location")
            .and_then(|header| header.to_str().ok())
            .ok_or_else(|| "Update download redirect is missing a valid URL.".to_string())?;
        uri = validate_https_download_url(location)?;
    }
    Err("Too many redirects while downloading an update.".into())
}

/// Validates that `url` is a safe GitHub Releases web page URL for `expected_repo`.
fn validate_github_release_url(url: &str, expected_repo: &str) -> Result<(), String> {
    validate_repo(expected_repo)?;
    if url.is_empty()
        || url.bytes().any(|b| {
            b.is_ascii_control()
                || b.is_ascii_whitespace()
                || matches!(
                    b,
                    b'"' | b'\''
                        | b'<'
                        | b'>'
                        | b'`'
                        | b'\\'
                        | b'|'
                        | b'&'
                        | b';'
                        | b'$'
                        | b'%'
                        | b'^'
                )
        })
    {
        return Err("Refusing release URL with unsafe characters.".into());
    }
    let expected_prefix = format!(
        "https://github.com/{}/releases/",
        expected_repo.to_ascii_lowercase()
    );
    let normalized = url.to_ascii_lowercase();
    if !normalized.starts_with(&expected_prefix) {
        return Err(format!(
            "Refusing release URL outside {expected_repo}: {url}"
        ));
    }
    Ok(())
}

/// Queries GitHub's `/releases/latest` endpoint and evaluates whether an update is available.
fn fetch_latest_release(repo: &str, current_str: &str) -> Result<CheckOutcome, String> {
    validate_repo(repo)?;
    let current = SemVer::parse(current_str)
        .ok_or_else(|| format!("Invalid local version '{current_str}'."))?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let user_agent = format!("TackleCast/{current_str}");

    let agent = http_agent(API_TIMEOUT);
    let mut response = agent
        .get(&url)
        .header("User-Agent", &user_agent)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(format_http_error)?;
    if response.status() != ureq::http::StatusCode::OK {
        return Err(format!(
            "Unexpected GitHub release response: {}",
            response.status()
        ));
    }

    let body = read_bounded_utf8(
        response.body_mut().as_reader(),
        MAX_API_RESPONSE_BYTES,
        "GitHub release response",
    )?;

    let install_mode = current_exe_dir()
        .map(|dir| detect_install_mode(&dir))
        .unwrap_or_else(InstallMode::ExternalOnly);

    let public_key =
        option_env!("TACKLECAST_UPDATE_PUBLIC_KEY").and_then(|key| decode_hex(key.trim()));
    evaluate_release_json(
        &body,
        repo,
        current,
        current_platform_asset(),
        install_mode,
        public_key,
    )
}

/// Parses a GitHub latest-release JSON payload and determines the check outcome.
fn evaluate_release_json(
    json: &str,
    repo: &str,
    current: SemVer,
    platform_spec: Option<PlatformAssetSpec>,
    install_mode: InstallMode,
    public_key: Option<[u8; 32]>,
) -> Result<CheckOutcome, String> {
    let release: GitHubRelease =
        serde_json::from_str(json).map_err(|e| format!("Invalid GitHub release payload: {e}"))?;

    if release.draft || release.prerelease {
        return Ok(CheckOutcome::UpToDate {
            current: current.to_string(),
        });
    }

    let remote = SemVer::parse(&release.tag_name).ok_or_else(|| {
        format!(
            "Latest release tag '{}' is not a stable semantic version.",
            release.tag_name
        )
    })?;

    if remote <= current {
        return Ok(CheckOutcome::UpToDate {
            current: current.to_string(),
        });
    }

    validate_github_release_url(&release.html_url, repo)?;

    let (download_plan, fallback_reason) = match (platform_spec, install_mode, public_key) {
        (None, _, _) => (
            None,
            Some("No official self-update archive for this platform.".into()),
        ),
        (_, InstallMode::ExternalOnly(reason), _) => (None, Some(reason)),
        (_, _, None) => (
            None,
            Some("No update signing key is configured for this build.".into()),
        ),
        (Some(spec), InstallMode::PortableWritable(install_dir), Some(public_key)) => {
            let archive = release.assets.iter().find(|a| a.name == spec.archive_name);
            let checksum = release.assets.iter().find(|a| a.name == spec.checksum_name);
            let signature_name = format!("{}.sig", spec.checksum_name);
            let signature = release.assets.iter().find(|a| a.name == signature_name);
            match (archive, checksum, signature) {
                (Some(arch), Some(sum), Some(sig)) if arch.size <= MAX_ARCHIVE_BYTES => {
                    for asset in [arch, sum, sig] {
                        validate_release_asset(asset, repo, &release.tag_name)?;
                    }
                    (
                        Some(Box::new(DownloadPlan {
                            archive_name: arch.name.clone(),
                            archive_url: arch.browser_download_url.clone(),
                            checksum_url: sum.browser_download_url.clone(),
                            signature_url: sig.browser_download_url.clone(),
                            repo: repo.to_ascii_lowercase(),
                            tag: release.tag_name.clone(),
                            public_key,
                            expected_size: (arch.size > 0).then_some(arch.size),
                            kind: spec.kind,
                            install_dir,
                        })),
                        None,
                    )
                }
                (None, _, _) => (
                    None,
                    Some(format!(
                        "Release is missing {} for this platform.",
                        spec.archive_name
                    )),
                ),
                (_, None, _) | (_, _, None) => (
                    None,
                    Some("Release has no signed checksum for this platform.".into()),
                ),
                _ => (
                    None,
                    Some("The release archive is too large for in-app installation.".into()),
                ),
            }
        }
    };

    Ok(CheckOutcome::Available(PendingRelease {
        version: remote.to_string(),
        html_url: release.html_url,
        download_plan,
        fallback_reason,
    }))
}

/// Formats `ureq` transport and HTTP status errors into concise user-facing text.
fn format_http_error(err: ureq::Error) -> String {
    match err {
        ureq::Error::StatusCode(403) => {
            "GitHub API rate limit reached. Try again later or check GitHub directly.".into()
        }
        ureq::Error::StatusCode(404) => "No published GitHub releases found yet.".into(),
        ureq::Error::StatusCode(code) => format!("GitHub returned HTTP {code}."),
        other => format!("Update request failed: {other}"),
    }
}

/// Parses a `.sha256` file (`<64-hex>  <filename>`) and extracts the expected digest.
fn parse_sha256_file(contents: &str, expected_filename: &str) -> Option<String> {
    for line in contents.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut parts = trimmed.split_whitespace();
        let hash = parts.next()?;
        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        if let Some(name) = parts.next() {
            let normalized = name.trim_start_matches('*');
            let base = Path::new(normalized)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(normalized);
            if base != expected_filename {
                continue;
            }
        }
        return Some(hash.to_ascii_lowercase());
    }
    None
}

fn decode_hex<const N: usize>(hex: &str) -> Option<[u8; N]> {
    if hex.len() != N * 2 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let mut bytes = [0; N];
    for (byte, pair) in bytes.iter_mut().zip(hex.as_bytes().chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(bytes)
}

fn signed_checksum_message(repo: &str, tag: &str, archive: &str, hash: &str) -> String {
    format!("tacklecast-update-v1\n{repo}\n{tag}\n{archive}\n{hash}\n")
}

fn verify_signed_checksum(
    public_key: &[u8; 32],
    message: &str,
    signature: &[u8],
) -> Result<(), String> {
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(message.as_bytes(), signature)
        .map_err(|_| "Update signature did not match the trusted release key.".into())
}

fn download_and_stage(
    version: String,
    plan: DownloadPlan,
    cancel: Arc<AtomicBool>,
    tx: &Sender<WorkerMessage>,
    waker: &Arc<dyn Fn() + Send + Sync>,
) {
    let send = |msg: WorkerMessage| {
        let _ = tx.send(msg);
        (waker)();
    };
    send(match stage_release(version, &plan, &cancel, &send) {
        Ok(Some(staged)) => WorkerMessage::Staged(staged),
        Ok(None) => WorkerMessage::Cancelled,
        Err(_) if cancel.load(Ordering::Relaxed) => WorkerMessage::Cancelled,
        Err(err) => WorkerMessage::Failed(err),
    });
}

fn stage_release(
    version: String,
    plan: &DownloadPlan,
    cancel: &AtomicBool,
    send: &impl Fn(WorkerMessage),
) -> Result<Option<StagedUpdate>, String> {
    let user_agent = format!("TackleCast/{}", current_version());
    let expected_hash = fetch_signed_checksum(plan, &user_agent)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    let work = install::create_staging(&plan.install_dir)
        .map_err(|err| format!("Cannot create update staging directory: {err}"))?;
    let archive_path = work.path().join(&plan.archive_name);
    let agent = http_agent(Duration::from_secs(15 * 60));
    let mut response = open_asset(&agent, &plan.archive_url, &user_agent)?;
    let content_length = response
        .headers()
        .get("content-length")
        .map(|header| {
            header
                .to_str()
                .ok()
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| "Invalid update archive Content-Length.".to_string())
        })
        .transpose()?;
    let total = archive_total(plan.expected_size, content_length)?;
    let limit = total.unwrap_or(MAX_ARCHIVE_BYTES);

    let mut file = File::create_new(&archive_path)
        .map_err(|err| format!("Cannot create temporary archive: {err}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded = 0;
    let mut last_report = Instant::now();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let size = reader
            .read(&mut buf)
            .map_err(|err| format!("Download interrupted: {err}"))?;
        if size == 0 {
            break;
        }
        downloaded += size as u64;
        if downloaded > limit {
            return Err("Update download exceeded its allowed size.".into());
        }
        file.write_all(&buf[..size])
            .map_err(|err| format!("Download write failed: {err}"))?;
        hasher.update(&buf[..size]);
        if last_report.elapsed() >= Duration::from_millis(80) {
            send(WorkerMessage::DownloadProgress {
                version: version.clone(),
                downloaded,
                total,
            });
            last_report = Instant::now();
        }
    }
    drop(file);
    if total.is_some_and(|total| total != downloaded) {
        return Err("Update archive did not match the expected size.".into());
    }

    send(WorkerMessage::Verifying {
        version: version.clone(),
    });
    if format_hex(&hasher.finalize()) != expected_hash {
        return Err("SHA-256 checksum mismatch. The downloaded archive was discarded.".into());
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(None);
    }

    send(WorkerMessage::Extracting {
        version: version.clone(),
    });
    let payload = work.path().join("payload");
    archive::extract(plan.kind, &archive_path, &payload)
        .map_err(|err| format!("Cannot extract the update: {err}"))?;
    fs::remove_file(archive_path)
        .map_err(|err| format!("Cannot remove the unpacked archive: {err}"))?;
    if !fs::symlink_metadata(install::binary_path(&payload))
        .is_ok_and(|meta| install::is_regular_file(&meta))
        || install::read_version(&payload).ok() != SemVer::parse(&version)
    {
        return Err(
            "Update archive is missing the executable or has mismatched build metadata.".into(),
        );
    }
    Ok((!cancel.load(Ordering::Relaxed)).then_some(StagedUpdate { version, work }))
}

fn archive_total(expected: Option<u64>, received: Option<u64>) -> Result<Option<u64>, String> {
    if expected
        .zip(received)
        .is_some_and(|(expected, received)| expected != received)
    {
        return Err("Update archive size disagrees with the release metadata.".into());
    }
    let total = expected.or(received);
    if total.is_some_and(|total| total == 0 || total > MAX_ARCHIVE_BYTES) {
        return Err("Update archive exceeds the supported size limits.".into());
    }
    Ok(total)
}

fn fetch_signed_checksum(plan: &DownloadPlan, user_agent: &str) -> Result<String, String> {
    let agent = http_agent(API_TIMEOUT);
    let mut response = open_asset(&agent, &plan.checksum_url, user_agent)?;
    let text = read_bounded_utf8(
        response.body_mut().as_reader(),
        MAX_CHECKSUM_BYTES,
        "checksum file",
    )?;
    let hash = parse_sha256_file(&text, &plan.archive_name).ok_or_else(|| {
        format!(
            "Checksum file has no valid SHA-256 for {}.",
            plan.archive_name
        )
    })?;
    let mut response = open_asset(&agent, &plan.signature_url, user_agent)?;
    let signature = read_bounded(response.body_mut().as_reader(), 64, "update signature")?;
    let message = signed_checksum_message(&plan.repo, &plan.tag, &plan.archive_name, &hash);
    verify_signed_checksum(&plan.public_key, &message, &signature)?;
    Ok(hash)
}

/// Encodes a byte slice as lowercase hexadecimal.
fn format_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(&mut out, "{b:02x}");
    }
    out
}

/// Locks portable installs and recovers a pending transaction before loading settings.
pub fn prepare_startup() -> Result<Option<File>, String> {
    install::startup(&current_exe_dir()?).map_err(|err| err.to_string())
}

/// Handles the internal staged helper modes before normal application startup.
pub fn maybe_run_apply_update_from_args() -> Option<i32> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    install::run_helper(&args)
}

/// Opens a validated GitHub release URL in the user's default web browser.
pub fn open_release_page(url: &str) -> Result<(), String> {
    validate_github_release_url(url, update_repo())?;

    #[cfg(target_os = "windows")]
    {
        use windows::core::{w, HSTRING, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide_url = HSTRING::from(url);
        // SAFETY: `ShellExecuteW` receives valid NUL-terminated UTF-16 strings and
        // returns an `HINSTANCE` whose numeric value > 32 indicates success.
        let result = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                &wide_url,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        if (result.0 as isize) <= 32 {
            return Err(format!(
                "Failed opening browser (ShellExecuteW code {}).",
                result.0 as isize
            ));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("Failed opening browser: {e}"))
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Err("Opening browser links is unsupported on this platform.".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use flate2::write::GzEncoder;
    #[cfg(unix)]
    use flate2::Compression;
    use std::sync::atomic::AtomicUsize;

    /// Creates a unique temporary directory under the OS temp folder for a test.
    fn temp_test_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tacklecast-updater-test-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("create temp test dir");
        dir
    }

    #[test]
    fn semver_parses_stable_tags_and_rejects_prereleases() {
        assert_eq!(
            SemVer::parse("v3.0.0"),
            Some(SemVer {
                major: 3,
                minor: 0,
                patch: 0
            })
        );
        assert_eq!(
            SemVer::parse("3.12.4"),
            Some(SemVer {
                major: 3,
                minor: 12,
                patch: 4
            })
        );
        assert!(SemVer::parse("v3.0.1") > SemVer::parse("v3.0.0"));
        assert!(SemVer::parse("v3.1.0") > SemVer::parse("v3.0.99"));
        assert!(SemVer::parse("v2.9.9") < SemVer::parse("v3.0.0"));
        assert_eq!(SemVer::parse("v3.1.0-rc.1"), None);
        assert_eq!(SemVer::parse("diagnostics-linux-audio-drift-25"), None);
        assert_eq!(SemVer::parse("v3.0"), None);
    }

    #[test]
    fn sha256_file_parser_matches_standard_formats() {
        let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let standard = format!("{digest}  TackleCast-Linux-x86_64.tar.gz\n");
        assert_eq!(
            parse_sha256_file(&standard, "TackleCast-Linux-x86_64.tar.gz").as_deref(),
            Some(digest)
        );
        let binary_flag = format!("{} *TackleCast-Windows-x86_64.zip\n", digest.to_uppercase());
        assert_eq!(
            parse_sha256_file(&binary_flag, "TackleCast-Windows-x86_64.zip").as_deref(),
            Some(digest)
        );
        assert_eq!(
            parse_sha256_file(&standard, "TackleCast-Windows-x86_64.zip"),
            None
        );
    }

    #[test]
    fn release_evaluation_pairs_platform_assets_and_handles_dev_fallback() {
        let json = r#"{
            "tag_name": "v3.1.0",
            "html_url": "https://github.com/SaltedByte/TackleCast/releases/tag/v3.1.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {
                    "name": "TackleCast-Linux-x86_64.tar.gz",
                    "browser_download_url": "https://github.com/SaltedByte/TackleCast/releases/download/v3.1.0/TackleCast-Linux-x86_64.tar.gz",
                    "size": 12345
                },
                {
                    "name": "TackleCast-Linux-x86_64.tar.gz.sha256",
                    "browser_download_url": "https://github.com/SaltedByte/TackleCast/releases/download/v3.1.0/TackleCast-Linux-x86_64.tar.gz.sha256",
                    "size": 99
                },
                {
                    "name": "TackleCast-Linux-x86_64.tar.gz.sha256.sig",
                    "browser_download_url": "https://github.com/SaltedByte/TackleCast/releases/download/v3.1.0/TackleCast-Linux-x86_64.tar.gz.sha256.sig",
                    "size": 64
                }
            ]
        }"#;
        let spec = PlatformAssetSpec {
            archive_name: "TackleCast-Linux-x86_64.tar.gz",
            checksum_name: "TackleCast-Linux-x86_64.tar.gz.sha256",
            kind: ArchiveKind::TarGz,
        };
        let current = SemVer::parse("3.0.0").unwrap();

        let portable = evaluate_release_json(
            json,
            DEFAULT_UPDATE_REPO,
            current,
            Some(spec),
            InstallMode::PortableWritable(PathBuf::from("/tmp/install")),
            Some([1; 32]),
        )
        .unwrap();
        match portable {
            CheckOutcome::Available(rel) => {
                assert_eq!(rel.version, "3.1.0");
                assert!(rel.download_plan.is_some());
                assert!(rel.fallback_reason.is_none());
            }
            CheckOutcome::UpToDate { .. } => panic!("expected available update"),
        }

        let dev = evaluate_release_json(
            json,
            DEFAULT_UPDATE_REPO,
            current,
            Some(spec),
            InstallMode::ExternalOnly("dev build".into()),
            Some([1; 32]),
        )
        .unwrap();
        match dev {
            CheckOutcome::Available(rel) => {
                assert!(rel.download_plan.is_none());
                assert_eq!(rel.fallback_reason.as_deref(), Some("dev build"));
            }
            CheckOutcome::UpToDate { .. } => panic!("expected available update"),
        }

        let untrusted = evaluate_release_json(
            json,
            DEFAULT_UPDATE_REPO,
            current,
            Some(spec),
            InstallMode::PortableWritable(PathBuf::from("/tmp/install")),
            None,
        )
        .unwrap();
        let CheckOutcome::Available(untrusted) = untrusted else {
            panic!("expected browser fallback");
        };
        assert!(untrusted.download_plan.is_none());
        assert!(untrusted.fallback_reason.is_some());

        let wrong_release = json.replace("/download/v3.1.0/", "/download/v2.9.0/");
        assert!(evaluate_release_json(
            &wrong_release,
            DEFAULT_UPDATE_REPO,
            current,
            Some(spec),
            InstallMode::PortableWritable(PathBuf::from("/tmp/install")),
            Some([1; 32]),
        )
        .is_err());
    }

    #[test]
    fn url_validation_rejects_shell_chars_http_and_untrusted_hosts() {
        assert!(validate_github_release_url(
            "https://github.com/SaltedByte/TackleCast/releases/tag/v3.1.0",
            "SaltedByte/TackleCast"
        )
        .is_ok());
        assert!(validate_github_release_url(
            "https://github.com/saltedbyte/tacklecast/releases/tag/v3.1.0",
            "SaltedByte/TackleCast"
        )
        .is_ok());
        assert!(validate_github_release_url(
            "https://github.com/SaltedByte/TackleCast/releases/tag/v3.1.0&calc.exe",
            "SaltedByte/TackleCast"
        )
        .is_err());
        assert!(validate_github_release_url(
            "https://github.com/OtherUser/TackleCast/releases/tag/v3.1.0",
            "SaltedByte/TackleCast"
        )
        .is_err());
        assert!(validate_github_release_url(
            "http://github.com/SaltedByte/TackleCast/releases/tag/v3.1.0",
            "SaltedByte/TackleCast"
        )
        .is_err());

        assert!(validate_https_download_url(
            "https://github.com/SaltedByte/TackleCast/releases/download/v3.1.0/TackleCast-Windows-x86_64.zip"
        )
        .is_ok());
        assert!(validate_https_download_url(
            "https://objects.githubusercontent.com/github-production-release-asset/foo"
        )
        .is_ok());
        assert!(validate_https_download_url(
            "http://github.com/SaltedByte/TackleCast/releases/download/v3.1.0/a.zip"
        )
        .is_err());
        assert!(validate_https_download_url("https://github.com@evil.example/a.zip").is_err());
        assert!(validate_https_download_url("https://evil.example/a.zip").is_err());
        assert!(
            validate_https_download_url("https://raw.githubusercontent.com/project/a.zip").is_err()
        );
        assert!(validate_https_download_url(
            "https://release-assets.githubusercontent.com/asset?token=value"
        )
        .is_ok());
        assert_eq!(http_agent(API_TIMEOUT).config().max_redirects(), 0);
    }

    #[test]
    fn release_signature_binds_repository_version_platform_and_checksum() {
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let key = Ed25519KeyPair::from_seed_unchecked(&[7; 32]).unwrap();
        let public_key = key.public_key().as_ref().try_into().unwrap();
        let hash = "ab".repeat(32);
        let message =
            signed_checksum_message("saltedbyte/tacklecast", "v3.1.0", "release.zip", &hash);
        let signature = key.sign(message.as_bytes());
        assert!(verify_signed_checksum(public_key, &message, signature.as_ref()).is_ok());
        for altered in [
            message.replace("saltedbyte", "another-owner"),
            message.replace("3.1.0", "3.0.0"),
            message.replace("release.zip", "release.tar.gz"),
            message.replace(&hash, &"cd".repeat(32)),
        ] {
            assert!(verify_signed_checksum(public_key, &altered, signature.as_ref()).is_err());
        }
        assert!(verify_signed_checksum(&[1; 32], &message, signature.as_ref()).is_err());
        assert!(verify_signed_checksum(public_key, &message, &[]).is_err());
    }

    #[test]
    fn archive_sizes_must_fit_and_agree() {
        assert_eq!(archive_total(Some(3), None).unwrap(), Some(3));
        assert_eq!(archive_total(None, None).unwrap(), None);
        assert!(archive_total(Some(3), Some(4)).is_err());
        assert!(archive_total(None, Some(MAX_ARCHIVE_BYTES + 1)).is_err());
    }
    #[test]
    fn bounded_utf8_reader_rejects_oversized_payloads() {
        let ok = read_bounded_utf8(&b"hello"[..], 5, "test").unwrap();
        assert_eq!(ok, "hello");
        assert!(read_bounded_utf8(&b"hello!"[..], 5, "test").is_err());
    }

    #[test]
    fn zip_extraction_unpacks_flat_windows_layout_and_rejects_traversal() {
        let root = temp_test_dir("zip");
        let zip_path = root.join("release.zip");
        {
            let file = File::create(&zip_path).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.add_directory("logs/", options).unwrap();
            writer.start_file("TackleCast.exe", options).unwrap();
            writer.write_all(b"exe-bytes").unwrap();
            writer.start_file("rife/model.onnx", options).unwrap();
            writer.write_all(b"model-bytes").unwrap();
            writer.finish().unwrap();
        }
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        archive::extract(ArchiveKind::Zip, &zip_path, &out).unwrap();
        assert!(!out.join("logs").exists());
        assert_eq!(fs::read(out.join("TackleCast.exe")).unwrap(), b"exe-bytes");
        assert_eq!(
            fs::read(out.join("rife/model.onnx")).unwrap(),
            b"model-bytes"
        );

        let evil_zip = root.join("evil.zip");
        {
            let file = File::create(&evil_zip).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default();
            writer.start_file("../escape.txt", options).unwrap();
            writer.write_all(b"bad").unwrap();
            writer.finish().unwrap();
        }
        let evil_out = root.join("evil-out");
        fs::create_dir_all(&evil_out).unwrap();
        assert!(archive::extract(ArchiveKind::Zip, &evil_zip, &evil_out).is_err());

        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn tar_gz_extraction_strips_top_dir_preserves_symlinks_and_permissions() {
        let root = temp_test_dir("targz");
        let tar_path = root.join("TackleCast-Linux-x86_64.tar.gz");
        {
            let file = File::create(&tar_path).unwrap();
            let enc = GzEncoder::new(file, Compression::default());
            let mut builder = tar::Builder::new(enc);

            let mut bin_hdr = tar::Header::new_gnu();
            let bin_data = b"#!/bin/sh\necho hi\n";
            bin_hdr.set_size(bin_data.len() as u64);
            bin_hdr.set_mode(0o755);
            bin_hdr.set_cksum();
            builder
                .append_data(
                    &mut bin_hdr,
                    "TackleCast-Linux-x86_64/run.sh",
                    &bin_data[..],
                )
                .unwrap();

            let mut lib_hdr = tar::Header::new_gnu();
            let lib_data = b"so-bytes";
            lib_hdr.set_size(lib_data.len() as u64);
            lib_hdr.set_mode(0o644);
            lib_hdr.set_cksum();
            builder
                .append_data(
                    &mut lib_hdr,
                    "TackleCast-Linux-x86_64/lib/libavcodec.so.62.1",
                    &lib_data[..],
                )
                .unwrap();

            let mut link_hdr = tar::Header::new_gnu();
            link_hdr.set_entry_type(tar::EntryType::Symlink);
            link_hdr.set_size(0);
            link_hdr.set_mode(0o777);
            link_hdr.set_cksum();
            builder
                .append_link(
                    &mut link_hdr,
                    "TackleCast-Linux-x86_64/lib/libavcodec.so.62",
                    "libavcodec.so.62.1",
                )
                .unwrap();
            builder
                .append_link(
                    &mut link_hdr,
                    "TackleCast-Linux-x86_64/lib/libavcodec.so",
                    "libavcodec.so.62",
                )
                .unwrap();

            builder.into_inner().unwrap().finish().unwrap();
        }

        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        archive::extract(ArchiveKind::TarGz, &tar_path, &out).unwrap();

        assert_eq!(
            fs::read(out.join("run.sh")).unwrap(),
            b"#!/bin/sh\necho hi\n"
        );
        assert_eq!(
            fs::read(out.join("lib/libavcodec.so.62")).unwrap(),
            b"so-bytes"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(out.join("run.sh"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "run.sh must keep executable bit");
            let link_target = fs::read_link(out.join("lib/libavcodec.so")).unwrap();
            assert_eq!(link_target, Path::new("libavcodec.so.62"));
        }

        let _ = fs::remove_dir_all(&root);
    }
}
