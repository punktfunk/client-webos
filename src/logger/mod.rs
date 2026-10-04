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
use std::sync::Mutex;

use anyhow::{Context, Result};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, Writer};
use tracing_subscriber::fmt::time::FormatTime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

pub use launch::webos_sdk_override;
pub use level::resolved_level;
pub use ring::{recent_lines, set_ring_capture};
pub use sink::{latest_log_file, previous_log_file, MAX_LOG_BYTES};

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

/// The appender's writer thread, held here rather than by `run` so [`flush`] can reach it from the
/// panic hook: with `panic = "abort"` no destructor runs, and the PANIC line would die in the queue.
static GUARD: Mutex<Option<tracing_appender::non_blocking::WorkerGuard>> = Mutex::new(None);

/// Flushes the sink on drop. Hold it for the life of the process.
pub struct FlushOnDrop;

impl Drop for FlushOnDrop {
    fn drop(&mut self) {
        flush();
    }
}

/// Drains queued lines to the sink and stops the writer thread. Later events are dropped.
pub fn flush() {
    let guard = GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take();
    drop(guard);
}

/// Installs the global subscriber (file/TCP + ring, shared level filter).
pub fn init_subscriber(app_dir: &Path) -> Result<FlushOnDrop> {
    let sink = sink::open(app_dir).context("open log sink")?;
    let (writer, guard) = tracing_appender::non_blocking(sink);
    let level = resolved_level();
    level::install(level);
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .event_format(HostLogFormat)
        .with_filter(LevelFilter::from_level(level));
    // The ring layer is gated by its own `Filter` (see `ring::CaptureFilter`) so an
    // inactive overlay can't silence `fmt_layer`.
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(ring::layer())
        .init();
    *GUARD.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(guard);
    Ok(FlushOnDrop)
}
