//! Routes `tracing` to TCP (dev machine) or log file. Destination from argv[1] at
//! runtime (webOS SAM passes launch `params` as JSON argv), not compile-time.
//!
//! Two layers over one shared level filter: `fmt` writes every event to the sink
//! through a non-blocking appender, `ring` keeps the last few lines in memory for
//! the log-tail overlay. Submodules are leaves; only this one wires them together.
mod launch;
mod level;
mod ring;
mod sink;

use std::path::Path;

use anyhow::{Context, Result};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{reload, Layer};

pub use launch::{launch_level_override, webos_sdk_override};
pub use level::{current_level_override, resolved_level, set_level_override};
pub use ring::{recent_lines, set_ring_capture};
pub use sink::latest_log_file;

/// Host-console bundle format: `<RFC3339-Z> <LEVEL> <target> <message>`.
struct HostLogFormat;

impl<S, N> FormatEvent<S, N> for HostLogFormat
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &tracing_subscriber::fmt::FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        tracing_subscriber::fmt::time::SystemTime.format_time(&mut writer)?;
        let meta = event.metadata();
        write!(writer, " {:5} {} ", meta.level(), meta.target())?;
        ctx.field_format().format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

/// Installs the global subscriber (file/TCP + ring, shared level filter).
/// Returns `WorkerGuard` — must stay alive for the process lifetime.
pub fn init_subscriber(app_dir: &Path) -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let sink = sink::open(app_dir).context("open log sink")?;
    let (writer, guard) = tracing_appender::non_blocking(sink);
    let level = resolved_level();
    let (filter, handle) = reload::Layer::new(LevelFilter::from_level(level));
    level::install_handle(handle, level);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .event_format(HostLogFormat)
        .with_filter(filter);
    // The ring layer is gated by its own `Filter` (see `ring::CaptureFilter`) so an
    // inactive overlay can't silence `fmt_layer`.
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(ring::layer())
        .init();
    Ok(guard)
}
