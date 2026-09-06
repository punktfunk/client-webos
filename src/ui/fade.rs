//! Shared open/close fade bookkeeping for modal-like overlays — the pre-stream `App`'s
//! `Screen` modals and the in-stream disconnect dialog both use one of these so every
//! dialog in the app opens/closes on the same clock and curve instead of each re-deriving
//! it (see `docs/NOTES.md` for why that drifted apart once: the disconnect dialog and
//! stats overlay live in `main.rs`'s streaming loop, which has no `App`/`Screen` to hook).

use crate::ui::animation::{anim_frac, anim_frac_in};
use std::time::{Duration, Instant};

/// Shared fade-in/out duration for transient in-stream overlays (toast notifications,
/// the stats overlay, the log-tail overlay) — one curve so toggling any of them feels
/// the same.
pub const OVERLAY_FADE: Duration = Duration::from_millis(400);

/// A self-expiring overlay's opacity: opaque until `hold` has passed since `shown`, then a
/// `fade`-long fade out, and `None` once it is spent — the caller's cue to drop whatever it
/// was showing.
///
/// One curve for everything that puts itself away (the toast, the Home status line, a
/// modal's scroll indicator), for the same reason the modals share [`ModalFade`]: three
/// copies of this shape had already drifted into two different curves, one of them linear
/// for no stated reason.
pub fn hold_alpha(shown: Instant, hold: Duration, fade: Duration) -> Option<f32> {
    if shown.elapsed() >= hold + fade {
        return None;
    }
    // `Instant::elapsed` saturates at zero, so the fade's clock reads as unstarted — and the
    // alpha as a flat 1.0 — for the whole hold.
    Some(1.0 - anim_frac(Some(shown + hold), fade))
}

/// Modal open. Short: the card is the response to a keypress, and delay there reads as
/// a slow TV, not as an animation.
pub const MODAL_FADE: Duration = Duration::from_millis(75);

/// Modal close, when nothing is opening behind it. Slower than the open — nothing is
/// waiting on it, and outlasting the incoming card is what makes a modal-to-modal step
/// read as one replacing the other.
pub const MODAL_FADE_OUT: Duration = Duration::from_millis(150);

/// `T` is whatever the caller needs preserved while closing (e.g. which screen was
/// open) so the fade-out can keep rendering it after the live state has already moved
/// on — use `()` if there's only ever one thing this could be.
pub struct ModalFade<T = ()> {
    open_since: Option<Instant>,
    closing: Option<(Instant, T)>,
    /// Whether the close in flight has something opening behind it — see [`Self::close_cross`].
    cross: bool,
    /// The two clocks, fixed when the fade is constructed rather than passed per call: a
    /// caller that reads one alpha on the open duration and ticks on another retires a fade
    /// half way through it, which is precisely how the quit dialog drifted out of step with
    /// every other modal. Here that pair cannot be mismatched.
    open_dur: Duration,
    solo_close_dur: Duration,
}

impl<T: Copy + PartialEq> ModalFade<T> {
    /// A modal layer: [`MODAL_FADE`] in, [`MODAL_FADE_OUT`] out (an open behind it shortens
    /// the close back to the open's clock, see [`Self::close_cross`]).
    pub fn modal() -> Self {
        Self::new(MODAL_FADE, MODAL_FADE_OUT)
    }

    /// A transient in-stream overlay: [`OVERLAY_FADE`] both ways.
    pub fn overlay() -> Self {
        Self::new(OVERLAY_FADE, OVERLAY_FADE)
    }

    fn new(open_dur: Duration, solo_close_dur: Duration) -> Self {
        Self {
            open_since: None,
            closing: None,
            cross: false,
            open_dur,
            solo_close_dur,
        }
    }

    /// Starts (or restarts) the open fade. Leaves an in-flight close alone.
    pub fn open(&mut self) {
        self.open_since = Some(Instant::now());
    }

    /// `open`, but cancels any in-flight close (for single-overlay callers).
    pub fn reopen(&mut self) {
        self.open();
        self.closing = None;
    }

    /// Starts the close fade, carrying `payload` for [`Self::closing_frame`] to hand back.
    pub fn close(&mut self, payload: T) {
        self.closing = Some((Instant::now(), payload));
        self.cross = false;
    }

    /// [`Self::close`] for an overlay with another one opening behind it: the two are made
    /// each other's exact inverse (see [`Self::closing_frame_against`]) on one clock
    /// ([`Self::close_dur`]).
    ///
    /// Their own curves do not compose — an ease-out closing against an ease-in opening sums
    /// to a quarter at the halfway point, and whatever is behind both shows through the seam.
    pub fn close_cross(&mut self, payload: T) {
        self.close(payload);
        self.cross = true;
    }

    /// How long the close in flight runs: a cross-fade matches the open exactly, a lone close
    /// takes the slower dissolve — there is nothing arriving to hide it.
    fn close_dur(&self) -> Duration {
        if self.cross {
            self.open_dur
        } else {
            self.solo_close_dur
        }
    }

    /// Cancels an in-flight close only if it's fading out `payload`.
    pub fn cancel_closing(&mut self, payload: T) {
        if self.closing.is_some_and(|(_, p)| p == payload) {
            self.closing = None;
        }
    }

    /// Returns `(alpha, payload)` while a close is in flight; `None` otherwise.
    pub fn closing_frame(&self) -> Option<(f32, T)> {
        let dur = self.close_dur();
        let (t, payload) = self.closing.filter(|(t, _)| t.elapsed() < dur)?;
        Some((1.0 - anim_frac(Some(t), dur), payload))
    }

    /// [`Self::closing_frame`] for the one caller that draws a cross-fade: `open` is the alpha
    /// of whatever is opening this frame, and a cross-fade's closing overlay is its inverse
    /// rather than its own ease, so the pair sums to one at every instant.
    pub fn closing_frame_against(&self, open: f32) -> Option<(f32, T)> {
        match self.closing_frame()? {
            (_, payload) if self.cross => Some((1.0 - open, payload)),
            frame => Some(frame),
        }
    }

    /// Whether a close is in flight, for callers that need its presence but not its alpha.
    pub fn is_closing(&self) -> bool {
        let dur = self.close_dur();
        self.closing.is_some_and(|(t, _)| t.elapsed() < dur)
    }

    /// Open-fade alpha: eases 0.0 -> 1.0, `1.0` once finished or if never opened.
    /// Ease-*in*, so it is `closing_frame`'s curve played backwards.
    pub fn open_alpha(&self) -> f32 {
        anim_frac_in(self.open_since, self.open_dur)
    }

    /// Alpha for a simple show/hide overlay driven by `shown`: `Some` through the close
    /// fade even after `shown` has already flipped to `false` (so the last frame doesn't
    /// cut instantly), and `None` once fully faded and hidden.
    pub fn visibility_alpha(&self, shown: bool) -> Option<f32> {
        if let Some((alpha, _)) = self.closing_frame() {
            return Some(alpha);
        }
        shown.then(|| self.open_alpha())
    }

    /// Whether an open or close fade is still mid-flight (i.e. hasn't yet reached its
    /// steady state). Pure and non-mutating, unlike `tick` — safe to call just to pick a
    /// redraw cadence.
    pub fn is_animating(&self) -> bool {
        self.open_since.is_some_and(|t| t.elapsed() < self.open_dur) || self.is_closing()
    }

    /// Advances the clock; returns whether either fade is still in flight. Each direction
    /// clears on its own duration, or the shorter keeps asking for dead redraws.
    pub fn tick(&mut self) -> bool {
        let close_dur = self.close_dur();
        let mut animating = false;
        if let Some(t) = self.open_since {
            if t.elapsed() >= self.open_dur {
                self.open_since = None;
            }
            animating = true;
        }
        if let Some((t, _)) = self.closing {
            if t.elapsed() >= close_dur {
                self.closing = None;
            }
            animating = true;
        }
        animating
    }
}
