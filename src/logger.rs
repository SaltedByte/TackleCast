use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt::time::OffsetTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

pub fn init_logging() -> Result<(), Box<dyn std::error::Error>> {
    let logs_dir = logs_dir();
    fs::create_dir_all(&logs_dir)?;
    prune_old_logs(&logs_dir)?;

    let file_path = logs_dir.join(format!(
        "tacklecast_{}.log",
        OffsetDateTime::now_local()
            .unwrap_or_else(|_| OffsetDateTime::now_utc())
            .format(&format_description!("[year][month][day]_[hour][minute][second]"))?
    ));

    let file = File::create(file_path)?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let _ = LOG_GUARD.set(guard);

    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    let timer = OffsetTime::new(
        offset,
        format_description!("[year]-[month]-[day] [hour]:[minute]:[second]"),
    );

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_target(false)
        .with_timer(timer)
        .with_writer(move || writer.clone())
        .with_filter(default_log_filter());

    tracing_subscriber::registry().with(file_layer).try_init()?;
    
    std::panic::set_hook(Box::new(|info| {
        tracing::error!("panic: {info}");
    }));
    
    Ok(())
}

fn default_log_filter() -> EnvFilter {
    if let Ok(filter) = EnvFilter::try_from_default_env() {
        return filter;
    }

    let mut filter = EnvFilter::new("info");
    for directive in [
        "tacklecast=info",
        "wgpu_hal=warn",
        "wgpu_core=warn",
        "wgpu=warn",
        "naga=warn",
        "egui_wgpu=warn",
        "ash=warn",
    ] {
        if let Ok(parsed) = directive.parse() {
            filter = filter.add_directive(parsed);
        }
    }
    filter
}

fn prune_old_logs(logs_dir: &Path) -> std::io::Result<()> {
    let mut entries: Vec<_> = fs::read_dir(logs_dir)?
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("tacklecast_")
                && entry.file_name().to_string_lossy().ends_with(".log")
        })
        .collect();

    entries.sort_by_key(|entry| entry.metadata().and_then(|meta| meta.modified()).ok());
    let remove_count = entries.len().saturating_sub(5);
    for entry in entries.into_iter().take(remove_count) {
        let _ = fs::remove_file(entry.path());
    }

    Ok(())
}

fn logs_dir() -> PathBuf {
    if cfg!(debug_assertions) {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("logs")
    } else {
        std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("logs")
    }
}
