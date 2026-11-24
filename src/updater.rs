//! Manual GitHub Releases self-updater for TackleCast.
//!
//! Queries GitHub's stable `/releases/latest` endpoint only when explicitly
//! requested by the user, verifies the matching platform archive against its
//! published `.sha256` checksum, stages extraction into a temporary directory
//! next to the executable, and hands off file replacement to the staged binary
//! via `--apply-update` with backup and rollback.

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tar::Archive;
use tracing::{info, warn};
use zip::ZipArchive;

/// Default upstream GitHub repository queried for official TackleCast releases.
pub const DEFAULT_UPDATE_REPO: &str = "SaltedByte/TackleCast";

/// Staging directory name created inside the portable install folder.
const STAGING_DIR_NAME: &str = ".update-staging";

/// Backup directory name used during `--apply-update` for rollback on failure.
const BACKUP_DIR_NAME: &str = ".update-backup";

/// Internal CLI flag used to invoke the staged binary as an update helper.
const APPLY_UPDATE_FLAG: &str = "--apply-update";

/// Network timeout applied to GitHub API and checksum requests.
const API_TIMEOUT: Duration = Duration::from_secs(15);

/// Maximum wait time for the parent TackleCast process to exit before applying.
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(15);

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
    if !exe_dir.join("build-info.json").is_file() {
        return InstallMode::ExternalOnly(
            "Portable release required for in-app update (dev/source build detected).".into(),
        );
    }
    let probe = exe_dir.join(format!(".tacklecast-write-probe-{}", std::process::id()));
    match File::create(&probe) {
        Ok(file) => {
            drop(file);
            let _ = fs::remove_file(&probe);
            InstallMode::PortableWritable(exe_dir.to_path_buf())
        }
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
#[derive(Debug, Clone)]
struct PendingRelease {
    version: String,
    html_url: String,
    download_plan: Option<DownloadPlan>,
    fallback_reason: Option<String>,
}

/// Concrete download and staging plan for a self-updatable release.
#[derive(Debug, Clone)]
struct DownloadPlan {
    archive_name: String,
    archive_url: String,
    checksum_url: String,
    expected_size: Option<u64>,
    kind: ArchiveKind,
    install_dir: PathBuf,
}

/// Staged update ready to be applied on restart.
#[derive(Debug, Clone)]
struct StagedUpdate {
    version: String,
    staging_dir: PathBuf,
    install_dir: PathBuf,
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
        let Some(release) = self.pending.clone() else {
            return;
        };
        let Some(plan) = release.download_plan else {
            return;
        };

        self.cancel_flag = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel_flag);
        let waker = Arc::clone(&self.waker);
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.status = UpdateStatus::Downloading {
            version: release.version.clone(),
            downloaded: 0,
            total: plan.expected_size,
        };

        std::thread::spawn(move || {
            download_and_stage(release.version, plan, cancel, &tx, &waker);
        });
    }

    /// Signals an active download worker to stop and clean up partial files.
    pub fn cancel_download(&mut self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }

    /// Dismisses the status banner when no background task is actively running.
    pub fn dismiss(&mut self) {
        if !self.status.is_busy() {
            self.status = UpdateStatus::Idle;
        }
    }

    /// Spawns the staged binary in `--apply-update` mode so the caller can exit.
    pub fn launch_staged_updater(&mut self) -> Result<(), String> {
        let Some(staged) = self.staged.clone() else {
            return Err("No staged update is ready to apply.".into());
        };
        if let Err(err) = spawn_staged_helper(&staged) {
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

/// Builds a configured `ureq::Agent` with TackleCast's HTTPS-only and timeout settings.
fn http_agent(timeout: Option<Duration>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder().https_only(true);
    if let Some(t) = timeout {
        builder = builder.timeout_global(Some(t));
    } else {
        builder = builder
            .timeout_connect(Some(API_TIMEOUT))
            .timeout_recv_body(Some(Duration::from_secs(60)));
    }
    builder.build().into()
}

/// Reads at most `max_bytes` from `reader` and decodes it as UTF-8.
fn read_bounded_utf8<R: Read>(reader: R, max_bytes: u64, label: &str) -> Result<String, String> {
    let mut buf = Vec::new();
    reader
        .take(max_bytes + 1)
        .read_to_end(&mut buf)
        .map_err(|e| format!("Failed reading {label}: {e}"))?;
    if buf.len() as u64 > max_bytes {
        return Err(format!("{label} exceeded maximum allowed size."));
    }
    String::from_utf8(buf).map_err(|e| format!("Invalid UTF-8 in {label}: {e}"))
}

/// Validates that `url` is a safe HTTPS URL on `github.com` or `*.githubusercontent.com`.
fn validate_https_download_url(url: &str) -> Result<(), String> {
    if url.is_empty()
        || url
            .bytes()
            .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
    {
        return Err("Refusing download URL with control or whitespace characters.".into());
    }
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(format!("Refusing non-HTTPS download URL: {url}"));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') || authority.contains(':') {
        return Err(format!(
            "Refusing download URL with unexpected authority: {url}"
        ));
    }
    let host = authority.to_ascii_lowercase();
    let trusted = host == "github.com"
        || (host.ends_with(".githubusercontent.com")
            && host.len() > ".githubusercontent.com".len());
    if !trusted {
        return Err(format!("Refusing download URL from untrusted host: {host}"));
    }
    Ok(())
}

/// Validates that `url` is a safe GitHub Releases web page URL for `expected_repo`.
fn validate_github_release_url(url: &str, expected_repo: &str) -> Result<(), String> {
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
    let current = SemVer::parse(current_str)
        .ok_or_else(|| format!("Invalid local version '{current_str}'."))?;
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let user_agent = format!("TackleCast/{current_str}");

    let agent = http_agent(Some(API_TIMEOUT));
    let mut response = agent
        .get(&url)
        .header("User-Agent", &user_agent)
        .header("Accept", "application/vnd.github+json")
        .call()
        .map_err(format_http_error)?;

    let body = read_bounded_utf8(
        response.body_mut().as_reader(),
        MAX_API_RESPONSE_BYTES,
        "GitHub release response",
    )?;

    let install_mode = current_exe_dir()
        .map(|dir| detect_install_mode(&dir))
        .unwrap_or_else(InstallMode::ExternalOnly);

    evaluate_release_json(&body, repo, current, current_platform_asset(), install_mode)
}

/// Parses a GitHub latest-release JSON payload and determines the check outcome.
fn evaluate_release_json(
    json: &str,
    repo: &str,
    current: SemVer,
    platform_spec: Option<PlatformAssetSpec>,
    install_mode: InstallMode,
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

    let (download_plan, fallback_reason) = match (platform_spec, install_mode) {
        (None, _) => (
            None,
            Some("No official self-update archive for this platform.".into()),
        ),
        (_, InstallMode::ExternalOnly(reason)) => (None, Some(reason)),
        (Some(spec), InstallMode::PortableWritable(install_dir)) => {
            let archive = release.assets.iter().find(|a| a.name == spec.archive_name);
            let checksum = release.assets.iter().find(|a| a.name == spec.checksum_name);
            match (archive, checksum) {
                (Some(arch), Some(sum)) => {
                    validate_https_download_url(&arch.browser_download_url)?;
                    validate_https_download_url(&sum.browser_download_url)?;
                    (
                        Some(DownloadPlan {
                            archive_name: arch.name.clone(),
                            archive_url: arch.browser_download_url.clone(),
                            checksum_url: sum.browser_download_url.clone(),
                            expected_size: (arch.size > 0).then_some(arch.size),
                            kind: spec.kind,
                            install_dir,
                        }),
                        None,
                    )
                }
                (None, _) => (
                    None,
                    Some(format!(
                        "Release is missing {} for this platform.",
                        spec.archive_name
                    )),
                ),
                (Some(_), None) => (
                    None,
                    Some(format!(
                        "Release is missing {} checksum; refusing automatic install.",
                        spec.checksum_name
                    )),
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

/// Downloads the archive and checksum, verifies SHA-256, and extracts into staging.
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

    let staging_dir = plan.install_dir.join(STAGING_DIR_NAME);
    if staging_dir.exists() {
        let _ = fs::remove_dir_all(&staging_dir);
    }
    if let Err(e) = fs::create_dir_all(&staging_dir) {
        send(WorkerMessage::Failed(format!(
            "Cannot create update staging directory: {e}"
        )));
        return;
    }

    let archive_path = staging_dir.join(&plan.archive_name);
    let user_agent = format!("TackleCast/{}", current_version());

    // Fetch expected SHA-256 checksum first so missing/invalid checksums fail fast.
    let expected_hash = match fetch_checksum(&plan.checksum_url, &plan.archive_name, &user_agent) {
        Ok(hash) => hash,
        Err(err) => {
            let _ = fs::remove_dir_all(&staging_dir);
            send(WorkerMessage::Failed(err));
            return;
        }
    };

    if cancel.load(Ordering::Relaxed) {
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Cancelled);
        return;
    }

    if let Err(err) = validate_https_download_url(&plan.archive_url) {
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Failed(err));
        return;
    }

    // Stream archive download while computing SHA-256 and reporting progress.
    let agent = http_agent(None);
    let mut response = match agent
        .get(&plan.archive_url)
        .header("User-Agent", &user_agent)
        .call()
    {
        Ok(resp) => resp,
        Err(err) => {
            let _ = fs::remove_dir_all(&staging_dir);
            send(WorkerMessage::Failed(format_http_error(err)));
            return;
        }
    };

    let total = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .or(plan.expected_size);

    let mut file = match File::create(&archive_path) {
        Ok(f) => f,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging_dir);
            send(WorkerMessage::Failed(format!(
                "Cannot create temporary archive file: {e}"
            )));
            return;
        }
    };

    let mut reader = response.body_mut().as_reader();
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    let mut downloaded: u64 = 0;
    let mut last_report = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_dir_all(&staging_dir);
            send(WorkerMessage::Cancelled);
            return;
        }
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Err(e) = file.write_all(&buf[..n]) {
                    drop(file);
                    let _ = fs::remove_dir_all(&staging_dir);
                    send(WorkerMessage::Failed(format!("Download write failed: {e}")));
                    return;
                }
                hasher.update(&buf[..n]);
                downloaded += n as u64;
                if last_report.elapsed() >= Duration::from_millis(80) {
                    send(WorkerMessage::DownloadProgress {
                        version: version.clone(),
                        downloaded,
                        total,
                    });
                    last_report = Instant::now();
                }
            }
            Err(e) => {
                drop(file);
                let _ = fs::remove_dir_all(&staging_dir);
                send(WorkerMessage::Failed(format!("Download interrupted: {e}")));
                return;
            }
        }
    }

    if let Err(e) = file.flush() {
        drop(file);
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Failed(format!(
            "Failed flushing downloaded archive: {e}"
        )));
        return;
    }
    drop(file);

    send(WorkerMessage::Verifying {
        version: version.clone(),
    });
    let actual_hash = format_hex(&hasher.finalize());
    if actual_hash != expected_hash {
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Failed(
            "SHA-256 checksum mismatch; downloaded archive was discarded.".into(),
        ));
        return;
    }

    send(WorkerMessage::Extracting {
        version: version.clone(),
    });
    let extract_result = match plan.kind {
        ArchiveKind::Zip => extract_zip_safely(&archive_path, &staging_dir),
        ArchiveKind::TarGz => extract_tar_gz_safely(&archive_path, &staging_dir),
    };
    let _ = fs::remove_file(&archive_path);

    if let Err(err) = extract_result {
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Failed(err));
        return;
    }

    if !staged_binary_path(&staging_dir).is_file() {
        let _ = fs::remove_dir_all(&staging_dir);
        send(WorkerMessage::Failed(
            "Extracted release archive is missing the TackleCast executable.".into(),
        ));
        return;
    }

    send(WorkerMessage::Staged(StagedUpdate {
        version,
        staging_dir,
        install_dir: plan.install_dir,
    }));
}

/// Downloads and parses the `.sha256` checksum asset for `archive_name`.
fn fetch_checksum(url: &str, archive_name: &str, user_agent: &str) -> Result<String, String> {
    validate_https_download_url(url)?;
    let agent = http_agent(Some(API_TIMEOUT));
    let mut response = agent
        .get(url)
        .header("User-Agent", user_agent)
        .call()
        .map_err(format_http_error)?;
    let text = read_bounded_utf8(
        response.body_mut().as_reader(),
        MAX_CHECKSUM_BYTES,
        "checksum file",
    )?;
    parse_sha256_file(&text, archive_name)
        .ok_or_else(|| format!("Checksum file did not contain a valid SHA-256 for {archive_name}."))
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

/// Validates a relative archive entry path and rejects path traversal components.
fn sanitize_relative_path(raw: &Path) -> Result<PathBuf, String> {
    let mut clean = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::Normal(part) => clean.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Archive entry contains an unsafe path: {}",
                    raw.display()
                ));
            }
        }
    }
    Ok(clean)
}

/// Strips a single shared top-level wrapper folder (e.g. `TackleCast-Linux-x86_64/`)
/// when every entry in the archive starts with the same first path component.
fn common_top_level_dir(paths: &[PathBuf]) -> Option<OsString> {
    let mut first_dir: Option<OsString> = None;
    let mut has_nested = false;

    for path in paths {
        let mut comps = path.components();
        let Some(Component::Normal(first)) = comps.next() else {
            return None;
        };
        if comps.next().is_some() {
            has_nested = true;
        }
        match &first_dir {
            None => first_dir = Some(first.to_os_string()),
            Some(existing) if existing == first => {}
            Some(_) => return None,
        }
    }

    if has_nested {
        first_dir
    } else {
        None
    }
}

/// Extracts a `.zip` archive into `dest` while rejecting path-traversal entries.
fn extract_zip_safely(archive_path: &Path, dest: &Path) -> Result<(), String> {
    let file = File::open(archive_path).map_err(|e| format!("Cannot open zip archive: {e}"))?;
    let mut archive = ZipArchive::new(file).map_err(|e| format!("Invalid zip archive: {e}"))?;

    let mut sanitized_paths = Vec::with_capacity(archive.len());
    for idx in 0..archive.len() {
        let entry = archive
            .by_index(idx)
            .map_err(|e| format!("Cannot inspect zip entry {idx}: {e}"))?;
        let rel = sanitize_relative_path(Path::new(entry.name()))?;
        if !rel.as_os_str().is_empty() {
            sanitized_paths.push(rel);
        }
    }
    let strip_prefix = common_top_level_dir(&sanitized_paths);

    for idx in 0..archive.len() {
        let mut entry = archive
            .by_index(idx)
            .map_err(|e| format!("Cannot read zip entry {idx}: {e}"))?;
        let mut rel = sanitize_relative_path(Path::new(entry.name()))?;
        if let Some(prefix) = &strip_prefix {
            if let Ok(stripped) = rel.strip_prefix(prefix) {
                rel = stripped.to_path_buf();
            }
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed creating directory {}: {e}", out_path.display()))?;
        } else {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating directory {}: {e}", parent.display()))?;
            }
            let mut out = File::create(&out_path)
                .map_err(|e| format!("Failed creating {}: {e}", out_path.display()))?;
            io::copy(&mut entry, &mut out)
                .map_err(|e| format!("Failed extracting {}: {e}", out_path.display()))?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode & 0o777));
            }
        }
    }
    Ok(())
}

/// Extracts a `.tar.gz` archive into `dest`, preserving Unix permissions and safe relative symlinks.
fn extract_tar_gz_safely(archive_path: &Path, dest: &Path) -> Result<(), String> {
    // Pass 1: validate every entry path and detect a shared top-level wrapper folder.
    let mut sanitized_paths = Vec::new();
    {
        let file =
            File::open(archive_path).map_err(|e| format!("Cannot open tar.gz archive: {e}"))?;
        let mut archive = Archive::new(GzDecoder::new(file));
        let entries = archive
            .entries()
            .map_err(|e| format!("Invalid tar.gz archive: {e}"))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("Corrupt tar.gz entry: {e}"))?;
            let path = entry
                .path()
                .map_err(|e| format!("Invalid tar.gz entry path: {e}"))?;
            let rel = sanitize_relative_path(&path)?;
            if !rel.as_os_str().is_empty() {
                sanitized_paths.push(rel);
            }
        }
    }
    let strip_prefix = common_top_level_dir(&sanitized_paths);

    // Pass 2: extract entries safely into `dest`.
    let file = File::open(archive_path).map_err(|e| format!("Cannot open tar.gz archive: {e}"))?;
    let mut archive = Archive::new(GzDecoder::new(file));
    let entries = archive
        .entries()
        .map_err(|e| format!("Invalid tar.gz archive: {e}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|e| format!("Corrupt tar.gz entry: {e}"))?;
        let raw_path = entry
            .path()
            .map_err(|e| format!("Invalid tar.gz entry path: {e}"))?
            .into_owned();
        let mut rel = sanitize_relative_path(&raw_path)?;
        if let Some(prefix) = &strip_prefix {
            if let Ok(stripped) = rel.strip_prefix(prefix) {
                rel = stripped.to_path_buf();
            }
        }
        if rel.as_os_str().is_empty() {
            continue;
        }
        let out_path = dest.join(&rel);
        let entry_type = entry.header().entry_type();

        if entry_type.is_dir() {
            fs::create_dir_all(&out_path)
                .map_err(|e| format!("Failed creating directory {}: {e}", out_path.display()))?;
        } else if entry_type.is_file() {
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating directory {}: {e}", parent.display()))?;
            }
            let mut out = File::create(&out_path)
                .map_err(|e| format!("Failed creating {}: {e}", out_path.display()))?;
            io::copy(&mut entry, &mut out)
                .map_err(|e| format!("Failed extracting {}: {e}", out_path.display()))?;
            #[cfg(unix)]
            if let Ok(mode) = entry.header().mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&out_path, fs::Permissions::from_mode(mode & 0o777));
            }
        } else if entry_type.is_symlink() {
            let link_target = entry
                .link_name()
                .map_err(|e| format!("Invalid symlink entry: {e}"))?
                .ok_or_else(|| format!("Symlink {} is missing target.", rel.display()))?
                .into_owned();
            validate_relative_symlink(&rel, &link_target)?;
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating directory {}: {e}", parent.display()))?;
            }
            #[cfg(unix)]
            {
                if out_path.symlink_metadata().is_ok() {
                    let _ = fs::remove_file(&out_path);
                }
                std::os::unix::fs::symlink(&link_target, &out_path).map_err(|e| {
                    format!(
                        "Failed creating symlink {} -> {}: {e}",
                        out_path.display(),
                        link_target.display()
                    )
                })?;
            }
            #[cfg(not(unix))]
            {
                let _ = (&out_path, &link_target);
            }
        }
    }
    Ok(())
}

/// Ensures a symlink inside an archive is relative and never resolves outside the staging root.
fn validate_relative_symlink(entry_rel: &Path, link_target: &Path) -> Result<(), String> {
    if link_target.is_absolute() {
        return Err(format!(
            "Archive symlink {} points to an absolute path: {}",
            entry_rel.display(),
            link_target.display()
        ));
    }
    let mut depth: i32 = 0;
    if let Some(parent) = entry_rel.parent() {
        for comp in parent.components() {
            if matches!(comp, Component::Normal(_)) {
                depth += 1;
            }
        }
    }
    for comp in link_target.components() {
        match comp {
            Component::CurDir => {}
            Component::Normal(_) => depth += 1,
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(format!(
                        "Archive symlink {} escapes staging directory: {}",
                        entry_rel.display(),
                        link_target.display()
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "Archive symlink {} uses an unsafe target: {}",
                    entry_rel.display(),
                    link_target.display()
                ));
            }
        }
    }
    Ok(())
}

/// Returns the expected TackleCast executable path inside `dir` for the current OS.
fn staged_binary_path(dir: &Path) -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        dir.join("TackleCast.exe")
    }
    #[cfg(not(target_os = "windows"))]
    {
        dir.join("tacklecast")
    }
}

/// Spawns the newly staged executable in `--apply-update` mode.
fn spawn_staged_helper(staged: &StagedUpdate) -> Result<(), String> {
    let helper_bin = staged_binary_path(&staged.staging_dir);
    let mut cmd = Command::new(&helper_bin);
    cmd.arg(APPLY_UPDATE_FLAG)
        .arg("--pid")
        .arg(std::process::id().to_string())
        .arg("--source")
        .arg(&staged.staging_dir)
        .arg("--target")
        .arg(&staged.install_dir)
        .current_dir(&staged.staging_dir);

    #[cfg(target_os = "linux")]
    {
        let lib_dir = staged.staging_dir.join("lib");
        if lib_dir.is_dir() {
            let value = match std::env::var_os("LD_LIBRARY_PATH") {
                Some(existing) if !existing.is_empty() => {
                    let mut combined = OsString::from(lib_dir.as_os_str());
                    combined.push(":");
                    combined.push(existing);
                    combined
                }
                _ => OsString::from(lib_dir.as_os_str()),
            };
            cmd.env("LD_LIBRARY_PATH", value);
        }
    }

    cmd.spawn().map(|_| ()).map_err(|e| {
        format!(
            "Failed launching staged updater {}: {e}",
            helper_bin.display()
        )
    })
}

/// Parsed arguments for the internal `--apply-update` helper mode.
#[derive(Debug, PartialEq, Eq)]
struct ApplyUpdateArgs {
    pid: u32,
    source: PathBuf,
    target: PathBuf,
}

/// Parses `--apply-update --pid <pid> --source <dir> --target <dir>` from raw OS args.
fn parse_apply_update_args(args: &[OsString]) -> Option<Result<ApplyUpdateArgs, String>> {
    if !args.iter().any(|arg| arg == APPLY_UPDATE_FLAG) {
        return None;
    }
    let mut pid: Option<u32> = None;
    let mut source: Option<PathBuf> = None;
    let mut target: Option<PathBuf> = None;

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];
        if arg == "--pid" {
            i += 1;
            let val = args.get(i).and_then(|s| s.to_str());
            pid = val.and_then(|s| s.parse::<u32>().ok());
        } else if arg == "--source" {
            i += 1;
            source = args.get(i).map(PathBuf::from);
        } else if arg == "--target" {
            i += 1;
            target = args.get(i).map(PathBuf::from);
        }
        i += 1;
    }

    Some(match (pid, source, target) {
        (Some(pid), Some(source), Some(target)) => Ok(ApplyUpdateArgs {
            pid,
            source,
            target,
        }),
        _ => {
            Err("Invalid --apply-update arguments; expected --pid, --source, and --target.".into())
        }
    })
}

/// Intercepts `--apply-update` early in `main()` before GUI/capture startup.
///
/// Returns `Some(exit_code)` when running in helper mode, or `None` for normal startup.
pub fn maybe_run_apply_update_from_args() -> Option<i32> {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let parsed = parse_apply_update_args(&args)?;
    let exit_code = match parsed {
        Ok(cfg) => match run_apply_update(&cfg) {
            Ok(()) => 0,
            Err(err) => {
                log_apply_error(&cfg.target, &err);
                1
            }
        },
        Err(err) => {
            eprintln!("tacklecast updater error: {err}");
            1
        }
    };
    Some(exit_code)
}

/// Executes the staged helper flow: wait for parent exit, overlay-copy with rollback, and relaunch.
fn run_apply_update(cfg: &ApplyUpdateArgs) -> Result<(), String> {
    wait_for_pid_exit(cfg.pid, PARENT_EXIT_TIMEOUT);
    let backup_dir = cfg.target.join(BACKUP_DIR_NAME);
    apply_staged_overlay(&cfg.source, &cfg.target, &backup_dir)?;
    relaunch_target(&cfg.target)
}

/// Appends an updater failure message to `<target>/logs/updater.log`.
fn log_apply_error(target_dir: &Path, message: &str) {
    let logs_dir = target_dir.join("logs");
    let _ = fs::create_dir_all(&logs_dir);
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(logs_dir.join("updater.log"))
    {
        let _ = writeln!(file, "Update apply failed: {message}");
    }
}

/// Waits until `pid` exits or `timeout` elapses.
fn wait_for_pid_exit(pid: u32, timeout: Duration) {
    #[cfg(target_os = "windows")]
    {
        use windows::Win32::Foundation::CloseHandle;
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        // SAFETY: `OpenProcess` returns a valid handle or an error; we wait and close it.
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
                let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
                let _ = WaitForSingleObject(handle, ms);
                let _ = CloseHandle(handle);
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        let proc_dir = PathBuf::from(format!("/proc/{pid}"));
        let deadline = Instant::now() + timeout;
        while proc_dir.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (pid, timeout);
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Overlay-copies all files and symlinks from `source` into `target` with backup and rollback.
///
/// Only paths present in `source` are touched in `target`; existing user files
/// such as `tacklecast_settings.json`, `logs/`, and `NvOFFRUC.dll` are never deleted.
fn apply_staged_overlay(source: &Path, target: &Path, backup: &Path) -> Result<(), String> {
    if backup.exists() {
        let _ = fs::remove_dir_all(backup);
    }
    fs::create_dir_all(backup)
        .map_err(|e| format!("Cannot create backup directory {}: {e}", backup.display()))?;

    let entries = collect_relative_entries(source)?;
    let mut backed_up: Vec<PathBuf> = Vec::new();
    let mut newly_created: Vec<PathBuf> = Vec::new();

    for rel in entries {
        let src_path = source.join(&rel);
        let dst_path = target.join(&rel);
        let bak_path = backup.join(&rel);

        let step = (|| -> Result<(), String> {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("Failed creating {}: {e}", parent.display()))?;
            }

            if let Ok(dst_meta) = dst_path.symlink_metadata() {
                if dst_meta.file_type().is_dir() {
                    return Err(format!(
                        "Refusing to replace existing directory {} with staged file.",
                        dst_path.display()
                    ));
                }
                if let Some(bak_parent) = bak_path.parent() {
                    fs::create_dir_all(bak_parent)
                        .map_err(|e| format!("Failed creating {}: {e}", bak_parent.display()))?;
                }
                move_with_retries(&dst_path, &bak_path)?;
                backed_up.push(rel.clone());
            } else {
                newly_created.push(rel.clone());
            }

            copy_entry(&src_path, &dst_path)
        })();

        if let Err(err) = step {
            rollback_overlay(target, backup, &backed_up, &newly_created);
            return Err(err);
        }
    }

    Ok(())
}

/// Recursively collects all file and symlink paths under `root` as relative paths.
fn collect_relative_entries(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    collect_relative_entries_inner(root, root, &mut out)?;
    out.sort();
    Ok(out)
}

/// Recursive helper for [`collect_relative_entries`].
fn collect_relative_entries_inner(
    root: &Path,
    current: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let dir = fs::read_dir(current)
        .map_err(|e| format!("Cannot read staging directory {}: {e}", current.display()))?;
    for entry in dir {
        let entry = entry.map_err(|e| format!("Failed iterating {}: {e}", current.display()))?;
        let name = entry.file_name();
        if name == STAGING_DIR_NAME || name == BACKUP_DIR_NAME {
            continue;
        }
        let path = entry.path();
        let meta = fs::symlink_metadata(&path)
            .map_err(|e| format!("Failed inspecting {}: {e}", path.display()))?;
        if meta.file_type().is_dir() {
            collect_relative_entries_inner(root, &path, out)?;
        } else {
            let rel = path
                .strip_prefix(root)
                .map_err(|e| format!("Internal path error: {e}"))?
                .to_path_buf();
            out.push(rel);
        }
    }
    Ok(())
}

/// Moves `from` to `to`, retrying briefly to tolerate Windows antivirus/handle release delays.
fn move_with_retries(from: &Path, to: &Path) -> Result<(), String> {
    let mut last_err = None;
    for attempt in 0..20 {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                if attempt < 19 {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Err(format!(
        "Failed backing up {} to {}: {}",
        from.display(),
        to.display(),
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".into())
    ))
}

/// Copies a regular file (preserving Unix permissions) or recreates a symlink.
fn copy_entry(src: &Path, dst: &Path) -> Result<(), String> {
    #[cfg_attr(not(unix), allow(unused_variables))]
    let meta = fs::symlink_metadata(src)
        .map_err(|e| format!("Failed reading metadata for {}: {e}", src.display()))?;

    #[cfg(unix)]
    if meta.file_type().is_symlink() {
        let target = fs::read_link(src)
            .map_err(|e| format!("Failed reading symlink {}: {e}", src.display()))?;
        if dst.symlink_metadata().is_ok() {
            let _ = fs::remove_file(dst);
        }
        return std::os::unix::fs::symlink(&target, dst).map_err(|e| {
            format!(
                "Failed creating symlink {} -> {}: {e}",
                dst.display(),
                target.display()
            )
        });
    }

    fs::copy(src, dst)
        .map_err(|e| format!("Failed copying {} to {}: {e}", src.display(), dst.display()))?;

    #[cfg(unix)]
    {
        let _ = fs::set_permissions(dst, meta.permissions());
    }

    Ok(())
}

/// Restores all backed-up files and removes newly created files after a failed overlay step.
fn rollback_overlay(
    target: &Path,
    backup: &Path,
    backed_up: &[PathBuf],
    newly_created: &[PathBuf],
) {
    let backed_set: HashSet<&PathBuf> = backed_up.iter().collect();
    for rel in newly_created.iter().rev() {
        if !backed_set.contains(rel) {
            let _ = fs::remove_file(target.join(rel));
        }
    }
    for rel in backed_up.iter().rev() {
        let dst = target.join(rel);
        let bak = backup.join(rel);
        if dst.symlink_metadata().is_ok() {
            let _ = fs::remove_file(&dst);
        }
        let _ = fs::rename(&bak, &dst);
    }
}

/// Relaunches TackleCast from `target_dir` after a successful overlay update.
fn relaunch_target(target_dir: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        let exe = target_dir.join("TackleCast.exe");
        Command::new(&exe)
            .current_dir(target_dir)
            .spawn()
            .map(|_| ())
            .map_err(|e| {
                format!(
                    "Update applied, but relaunch failed ({}): {e}",
                    exe.display()
                )
            })
    }
    #[cfg(target_os = "linux")]
    {
        let run_sh = target_dir.join("run.sh");
        if run_sh.is_file() {
            Command::new(&run_sh)
                .current_dir(target_dir)
                .spawn()
                .map(|_| ())
                .map_err(|e| {
                    format!(
                        "Update applied, but relaunch failed ({}): {e}",
                        run_sh.display()
                    )
                })
        } else {
            let bin = target_dir.join("tacklecast");
            let mut cmd = Command::new(&bin);
            cmd.current_dir(target_dir);
            let lib_dir = target_dir.join("lib");
            if lib_dir.is_dir() {
                cmd.env("LD_LIBRARY_PATH", &lib_dir);
            }
            cmd.spawn().map(|_| ()).map_err(|e| {
                format!(
                    "Update applied, but relaunch failed ({}): {e}",
                    bin.display()
                )
            })
        }
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = target_dir;
        Ok(())
    }
}

/// Removes leftover `.update-staging` and `.update-backup` folders on normal startup.
pub fn cleanup_stale_update_dirs() {
    let Ok(exe_dir) = current_exe_dir() else {
        return;
    };
    for name in [STAGING_DIR_NAME, BACKUP_DIR_NAME] {
        let path = exe_dir.join(name);
        if !path.exists() {
            continue;
        }
        // Retry briefly in case the helper process is still exiting on Windows.
        for attempt in 0..5 {
            match fs::remove_dir_all(&path) {
                Ok(()) => {
                    info!("cleaned up temporary update directory: {}", path.display());
                    break;
                }
                Err(err) if attempt == 4 => {
                    warn!(
                        "could not remove temporary update directory {}: {err}",
                        path.display()
                    );
                }
                Err(_) => std::thread::sleep(Duration::from_millis(150)),
            }
        }
    }
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
    use flate2::write::GzEncoder;
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
        )
        .unwrap();
        match dev {
            CheckOutcome::Available(rel) => {
                assert!(rel.download_plan.is_none());
                assert_eq!(rel.fallback_reason.as_deref(), Some("dev build"));
            }
            CheckOutcome::UpToDate { .. } => panic!("expected available update"),
        }
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
            writer.start_file("TackleCast.exe", options).unwrap();
            writer.write_all(b"exe-bytes").unwrap();
            writer.start_file("rife/model.onnx", options).unwrap();
            writer.write_all(b"model-bytes").unwrap();
            writer.finish().unwrap();
        }
        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        extract_zip_safely(&zip_path, &out).unwrap();
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
        assert!(extract_zip_safely(&evil_zip, &evil_out).is_err());

        let _ = fs::remove_dir_all(&root);
    }

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
                    "TackleCast-Linux-x86_64/lib/libavcodec.so.62",
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
                    "TackleCast-Linux-x86_64/lib/libavcodec.so",
                    "libavcodec.so.62",
                )
                .unwrap();

            builder.into_inner().unwrap().finish().unwrap();
        }

        let out = root.join("out");
        fs::create_dir_all(&out).unwrap();
        extract_tar_gz_safely(&tar_path, &out).unwrap();

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

        assert!(
            validate_relative_symlink(Path::new("lib/bad.so"), Path::new("../../etc/passwd"))
                .is_err()
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overlay_apply_preserves_user_files_and_replaces_release_files() {
        let root = temp_test_dir("overlay");
        let target = root.join("install");
        let staging = target.join(STAGING_DIR_NAME);
        let backup = target.join(BACKUP_DIR_NAME);
        fs::create_dir_all(target.join("logs")).unwrap();
        fs::create_dir_all(target.join("lib")).unwrap();
        fs::create_dir_all(staging.join("lib")).unwrap();

        // Existing install + user files.
        fs::write(target.join("tacklecast"), b"old-bin").unwrap();
        fs::write(target.join("build-info.json"), r#"{"version":"3.0.0"}"#).unwrap();
        fs::write(target.join("tacklecast_settings.json"), r#"{"volume":0.8}"#).unwrap();
        fs::write(target.join("logs/session.log"), b"user-log").unwrap();
        fs::write(target.join("NvOFFRUC.dll"), b"user-dll").unwrap();

        // Staged update files.
        fs::write(staging.join("tacklecast"), b"new-bin").unwrap();
        fs::write(staging.join("build-info.json"), r#"{"version":"3.1.0"}"#).unwrap();
        fs::write(staging.join("lib/libavcodec.so.62"), b"new-so").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("libavcodec.so.62", staging.join("lib/libavcodec.so")).unwrap();

        apply_staged_overlay(&staging, &target, &backup).unwrap();

        assert_eq!(fs::read(target.join("tacklecast")).unwrap(), b"new-bin");
        assert_eq!(
            fs::read_to_string(target.join("build-info.json")).unwrap(),
            r#"{"version":"3.1.0"}"#
        );
        assert_eq!(
            fs::read_to_string(target.join("tacklecast_settings.json")).unwrap(),
            r#"{"volume":0.8}"#
        );
        assert_eq!(
            fs::read(target.join("logs/session.log")).unwrap(),
            b"user-log"
        );
        assert_eq!(fs::read(target.join("NvOFFRUC.dll")).unwrap(), b"user-dll");
        #[cfg(unix)]
        assert_eq!(
            fs::read_link(target.join("lib/libavcodec.so")).unwrap(),
            Path::new("libavcodec.so.62")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overlay_rollback_restores_originals_when_copy_fails() {
        let root = temp_test_dir("rollback");
        let target = root.join("install");
        let staging = target.join(STAGING_DIR_NAME);
        let backup = target.join(BACKUP_DIR_NAME);
        fs::create_dir_all(&target).unwrap();
        fs::create_dir_all(&staging).unwrap();

        fs::write(target.join("a_first.bin"), b"original-a").unwrap();
        fs::write(staging.join("a_first.bin"), b"updated-a").unwrap();
        fs::write(staging.join("b_new.bin"), b"brand-new").unwrap();
        // Force a copy error on `z_conflict` by creating a directory at the target file path.
        fs::create_dir_all(target.join("z_conflict")).unwrap();
        fs::write(target.join("z_conflict/keep.txt"), b"dir-marker").unwrap();
        fs::write(staging.join("z_conflict"), b"file-over-dir").unwrap();

        let result = apply_staged_overlay(&staging, &target, &backup);
        assert!(
            result.is_err(),
            "overlay over non-empty directory must fail"
        );
        assert_eq!(
            fs::read(target.join("a_first.bin")).unwrap(),
            b"original-a",
            "backed-up file must be restored on rollback"
        );
        assert!(
            !target.join("b_new.bin").exists(),
            "newly created file must be removed on rollback"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn apply_update_args_preserve_case_sensitive_paths() {
        let raw = vec![
            OsString::from("--apply-update"),
            OsString::from("--pid"),
            OsString::from("4242"),
            OsString::from("--source"),
            OsString::from("/Home/User/TackleCast/.update-staging"),
            OsString::from("--target"),
            OsString::from("/Home/User/TackleCast"),
        ];
        let parsed = parse_apply_update_args(&raw).unwrap().unwrap();
        assert_eq!(parsed.pid, 4242);
        assert_eq!(
            parsed.source,
            PathBuf::from("/Home/User/TackleCast/.update-staging")
        );
        assert_eq!(parsed.target, PathBuf::from("/Home/User/TackleCast"));
    }
}
