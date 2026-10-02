//! Open/close fade clocks for the overlays: the stats card, toasts and the confirm dialog
//! share one set of curves so toggling any of them feels the same.

use std::time::{Duration, Instant};

/// Fade-in/out for transient in-stream overlays (toasts, the stats card).
pub(in crate::runtime) const OVERLAY_FADE: Duration = Duration::from_millis(400);

/// Dialog open. Short: the card answers a keypress, and delay there reads as a slow TV.
const MODAL_FADE: Duration = Duration::from_millis(75);

/// Dialog close. Slower than the open: nothing is waiting on it.
const MODAL_FADE_OUT: Duration = Duration::from_millis(150);

/// A self-expiring overlay's opacity: opaque until `hold` has passed since `shown`, then a
/// `fade`-long fade out, and `None` once it is spent.
pub(in crate::runtime) fn hold_alpha(shown: Instant, hold: Duration, fade: Duration) -> Option<f32> {
    if shown.elapsed() >= hold + fade {
        return None;
    }
    // `Instant::elapsed` saturates at zero, so the fade reads as unstarted for the whole hold.
    Some(1.0 - anim_frac(Some(shown + hold), fade))
}

/// Cubic ease-out progress 0..=1; 1.0 when done or absent.
pub(super) fn anim_frac(anim: Option<Instant>, dur: Duration) -> f32 {
    frac(anim, dur, |f| 1.0 - (1.0 - f).powi(3))
}

/// Cubic ease-in: the fade-in is the fade-out played backwards.
fn anim_frac_in(anim: Option<Instant>, dur: Duration) -> f32 {
    frac(anim, dur, |f| f.powi(3))
}

fn frac(anim: Option<Instant>, dur: Duration, curve: impl Fn(f32) -> f32) -> f32 {
    match anim {
        Some(t) => curve((t.elapsed().as_secs_f32() / dur.as_secs_f32()).min(1.0)),
        None => 1.0,
    }
}

/// `T` is what the close fade keeps drawing after the live state has moved on (the focused
/// button); `()` when there is only one thing it could be.
pub(in crate::runtime) struct ModalFade<T = ()> {
    open_since: Option<Instant>,
    closing: Option<(Instant, T)>,
    open_dur: Duration,
    close_dur: Duration,
}

impl<T: Copy> ModalFade<T> {
    /// A dialog: [`MODAL_FADE`] in, [`MODAL_FADE_OUT`] out.
    pub(super) fn modal() -> Self {
        Self::new(MODAL_FADE, MODAL_FADE_OUT)
    }

    /// A transient overlay: [`OVERLAY_FADE`] both ways.
    pub(in crate::runtime) fn overlay() -> Self {
        Self::new(OVERLAY_FADE, OVERLAY_FADE)
    }

    fn new(open_dur: Duration, close_dur: Duration) -> Self {
        Self {
            open_since: None,
            closing: None,
            open_dur,
            close_dur,
        }
    }

    /// Starts (or restarts) the open fade. Leaves an in-flight close alone.
    pub(in crate::runtime) fn open(&mut self) {
        self.open_since = Some(Instant::now());
    }

    /// `open`, but cancels any in-flight close.
    pub(in crate::runtime) fn reopen(&mut self) {
        self.open();
        self.closing = None;
    }

    /// Starts the close fade, carrying `payload` for [`Self::closing_frame`] to hand back.
    pub(in crate::runtime) fn close(&mut self, payload: T) {
        self.closing = Some((Instant::now(), payload));
    }

    /// `(alpha, payload)` while a close is in flight.
    pub(super) fn closing_frame(&self) -> Option<(f32, T)> {
        let (t, payload) = self.closing.filter(|(t, _)| t.elapsed() < self.close_dur)?;
        Some((1.0 - anim_frac(Some(t), self.close_dur), payload))
    }

    fn is_closing(&self) -> bool {
        self.closing.is_some_and(|(t, _)| t.elapsed() < self.close_dur)
    }

    /// Open-fade alpha: 0.0 -> 1.0, `1.0` once finished or never opened.
    pub(super) fn open_alpha(&self) -> f32 {
        anim_frac_in(self.open_since, self.open_dur)
    }

    /// Alpha for a show/hide overlay driven by `shown`: `Some` through the close fade even
    /// after `shown` flipped, `None` once fully hidden.
    pub(in crate::runtime) fn visibility_alpha(&self, shown: bool) -> Option<f32> {
        if let Some((alpha, _)) = self.closing_frame() {
            return Some(alpha);
        }
        shown.then(|| self.open_alpha())
    }

    /// Whether either fade is mid-flight. Non-mutating, for picking a redraw cadence.
    pub(in crate::runtime) fn is_animating(&self) -> bool {
        self.open_since.is_some_and(|t| t.elapsed() < self.open_dur) || self.is_closing()
    }

    /// Advances the clock; returns whether either fade is still in flight.
    pub(super) fn tick(&mut self) -> bool {
        let mut animating = false;
        if let Some(t) = self.open_since {
            if t.elapsed() >= self.open_dur {
                self.open_since = None;
            }
            animating = true;
        }
        if let Some((t, _)) = self.closing {
            if t.elapsed() >= self.close_dur {
                self.closing = None;
            }
            animating = true;
        }
        animating
    }
}
