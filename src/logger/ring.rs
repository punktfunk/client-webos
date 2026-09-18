//! In-memory ring of recent lines for the log-tail overlay, independent of the
//! file/TCP sink. Off by default: sessions not using the overlay pay one atomic
//! load per event.
use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::{Context, Filter};
use tracing_subscriber::Layer;

use super::level;

/// Bounds overlay memory and per-event lock scope.
const CAPACITY: usize = 32;
const LINE_MAX_CHARS: usize = 200;
/// Byte budget a line may reach while recording; chars are truncated exactly after.
const LINE_MAX_BYTES: usize = LINE_MAX_CHARS * 4;

static BUFFER: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

/// Whether the layer captures. Toggled by the Yellow button cycle.
static CAPTURE_ACTIVE: AtomicBool = AtomicBool::new(false);

fn buffer() -> &'static Mutex<VecDeque<String>> {
    BUFFER.get_or_init(|| Mutex::new(VecDeque::with_capacity(CAPACITY)))
}

/// Toggle ring-buffer capture on/off; stopping also clears the buffer.
pub fn set_ring_capture(active: bool) {
    CAPTURE_ACTIVE.store(active, Ordering::Relaxed);
    if !active {
        buffer().lock().unwrap_or_else(PoisonError::into_inner).clear();
    }
}

/// Last `n` log lines, oldest first, for the stream/menu overlay.
/// Clones the ring buffer without touching the file or TCP sink.
pub fn recent_lines(n: usize) -> Vec<String> {
    let buf = buffer().lock().unwrap_or_else(PoisonError::into_inner);
    let skip = buf.len().saturating_sub(n);
    let mut out = Vec::with_capacity(buf.len() - skip);
    out.extend(buf.iter().skip(skip).cloned());
    out
}

/// The ring layer, pre-gated by its capture filter — ready to hand to `registry()`.
pub(super) fn layer<S>() -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    RingLayer.with_filter(CaptureFilter)
}

/// Gates `RingLayer` via `Filter`, not `Layer::enabled`: the latter returning
/// `false` short-circuits the *entire* subscriber stack, so an inactive overlay (the
/// default) silenced the file/TCP sink too. A `Filter` disables only its own layer.
struct CaptureFilter;

impl<S> Filter<S> for CaptureFilter {
    fn enabled(&self, metadata: &tracing::Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        CAPTURE_ACTIVE.load(Ordering::Relaxed) && level::ordinal(*metadata.level()) <= level::current_ordinal()
    }

    /// Keep the hint bounded to the current level. An unbounded filter lowers
    /// tracing's global static max-level, forcing extra per-event callsite checks
    /// (down to `trace!`) instead of a cached `never`. `handle.modify` in
    /// `level::set_level_override` refreshes interest when this changes.
    fn max_level_hint(&self) -> Option<LevelFilter> {
        Some(level::ordinal_to_filter(level::current_ordinal()))
    }
}

/// Formats before taking the lock, holds it only for a bounded push/pop; zero I/O
/// in the render path.
struct RingLayer;

impl<S: tracing::Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        // ASCII lines fit exactly at this capacity, so the common case never reallocs.
        let mut visitor = LineVisitor(String::with_capacity(LINE_MAX_CHARS));
        let _ = write!(visitor.0, "{:<5} ", event.metadata().level());
        event.record(&mut visitor);
        let mut line = visitor.0;
        if let Some((cut, _)) = line.char_indices().nth(LINE_MAX_CHARS) {
            line.truncate(cut);
        }
        let mut buf = buffer().lock().unwrap_or_else(PoisonError::into_inner);
        if buf.len() >= CAPACITY {
            buf.pop_front();
        }
        buf.push_back(line);
    }
}

/// Appends straight into the final line buffer — one allocation per event, on
/// whatever thread logged (the video pump included). Stops recording once the
/// budget is spent so a fat `Debug` field can't balloon the string first.
struct LineVisitor(String);

impl tracing::field::Visit for LineVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if self.0.len() >= LINE_MAX_BYTES {
            return;
        }
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        } else {
            let _ = write!(self.0, " {}={value:?}", field.name());
        }
    }
}
