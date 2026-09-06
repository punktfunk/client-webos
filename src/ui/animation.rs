use crate::ui::render::Rect;
use std::time::{Duration, Instant};

/// How long a focused widget takes to pop to its zoomed size.
pub const FOCUS_POP: Duration = Duration::from_millis(140);

/// Grid card focus animation (zoom, glow, title wipe). One duration so all land together.
pub const CARD_FOCUS_POP: Duration = Duration::from_millis(160);

/// Held card's submenu panel rise time. Named separately because it covers several times
/// the title strip's travel; eased at both ends for one motion (not instant start).
pub const CARD_MENU_RISE: Duration = CARD_FOCUS_POP;

/// Per-16ms-tick scroll distance fraction. Unit for `ease_scroll` rate.
const SCROLL_STEP_PER_TICK: f64 = 0.35;
pub const SCROLL_STEP_TICK: Duration = Duration::from_millis(16);
/// Max frame dt before teleporting (app stalled; gap→whole distance reads as jump).
const SCROLL_MAX_DT: Duration = Duration::from_millis(64);

/// Ease scroll: cover ~35% per 16ms, snap when close. Returns true if moved.
/// Rate-based (not step-per-tick) so overruns only smooth, not speed.
pub fn ease_scroll(current: &mut i32, target: i32, dt: Duration) -> bool {
    let d = target - *current;
    if d == 0 {
        return false;
    }
    let ticks = dt.min(SCROLL_MAX_DT).as_secs_f64() / SCROLL_STEP_TICK.as_secs_f64();
    let covered = 1.0 - (1.0 - SCROLL_STEP_PER_TICK).powf(ticks);
    let step = if d.abs() <= 3 {
        d
    } else {
        match (f64::from(d) * covered) as i32 {
            0 => d.signum(),
            s => s,
        }
    };
    *current += step;
    true
}

/// How long a pressed button takes to spring back out of its dip.
pub const PRESS_POP: Duration = Duration::from_millis(120);

/// Press drop distance (px).
const PRESS_DROP: f32 = 5.0;
/// Focused widget tile growth (pop every focus tile rides).
pub const FOCUS_GROWTH: f32 = 0.02;
/// Button press-in animation (same everywhere; clock owned by App/ConfirmDialog).
#[derive(Default, Clone, Copy)]
pub struct Press(Option<Instant>);

impl Press {
    /// Start dip (visual only; action runs immediately).
    pub fn arm(&mut self) {
        self.0 = Some(Instant::now());
    }

    /// Dip in flight (frames owed while true).
    pub fn armed(self) -> bool {
        self.0.is_some()
    }

    /// Armed dip played all the way out.
    pub fn landed(self) -> bool {
        self.0.is_some_and(|t| t.elapsed() >= PRESS_POP)
    }

    /// Disarms, reporting whether anything was armed.
    pub fn take(&mut self) -> bool {
        self.0.take().is_some()
    }

    /// Base rect pushed down by press progress. Translation not scale (no resample).
    pub fn rect(self, base: Rect) -> Rect {
        base.offset(0, self.offset() as i32)
    }

    fn offset(self) -> f32 {
        PRESS_DROP * (1.0 - anim_frac(self.0, PRESS_POP))
    }
}

/// Modal card slide distance (px).
const MODAL_RISE: f32 = 26.0;
/// Modal layer offset at fade progress p (all modals/dialogs share for consistency).
pub fn modal_rise(p: f32) -> i32 {
    ((1.0 - p) * MODAL_RISE) as i32
}

/// Cubic ease-out.
pub fn ease(f: f32) -> f32 {
    1.0 - (1.0 - f).powi(3)
}
/// Eased at both ends (not instant start like ease).
pub fn smoothstep(f: f32) -> f32 {
    f * f * (3.0 - 2.0 * f)
}

/// Animation progress 0..=1; 1.0 when done/absent.
pub fn anim_frac(anim: Option<Instant>, dur: Duration) -> f32 {
    frac(anim, dur, ease)
}
/// Animation progress on cubic ease-in (mirror of ease; fade-in reads like backwards fade-out).
pub fn anim_frac_in(anim: Option<Instant>, dur: Duration) -> f32 {
    frac(anim, dur, |f| f.powi(3))
}
/// Animation progress on smoothstep (grid card focus: avoids snap-then-drift look).
pub fn anim_frac_smooth(anim: Option<Instant>, dur: Duration) -> f32 {
    frac(anim, dur, smoothstep)
}

fn frac(anim: Option<Instant>, dur: Duration, curve: impl Fn(f32) -> f32) -> f32 {
    // Clock read only when animating (most calls are None arm).
    match anim {
        Some(_) => frac_at(anim, dur, Instant::now(), curve),
        None => 1.0,
    }
}

/// [`frac`] against a clock the caller already read. Per-element loops (the grid's visible
/// cards) take one `Instant::now()` for the frame rather than one per element per curve.
fn frac_at(anim: Option<Instant>, dur: Duration, now: Instant, curve: impl Fn(f32) -> f32) -> f32 {
    match anim {
        // Saturating: a clock armed in the future (the reveal wave's stagger) has not started.
        Some(t) => curve((now.saturating_duration_since(t).as_secs_f32() / dur.as_secs_f32()).min(1.0)),
        None => 1.0,
    }
}

/// [`anim_frac`] on a caller-held clock. See [`frac_at`].
pub fn anim_frac_at(anim: Option<Instant>, dur: Duration, now: Instant) -> f32 {
    frac_at(anim, dur, now, ease)
}

/// A staggered fade across a surface: one curve, started up to `span` later depending on how
/// far along the sweep a given point sits. Whoever owns the surface decides what `progress`
/// means — a texel's position in an image — and the motion is the same either way, which is
/// the point: the launch backdrop's exit and the grid's reveal both evaluate one of these over
/// a small dissolve-mask texture, in the same direction.
///
/// Smoothstep rather than the cubic ease-out a pop uses: over a long fade `1-(1-t)³` is
/// near-opaque a sixth of the way through, which lands as a pop.
#[derive(Clone, Copy)]
pub struct Wave {
    /// How much later the far end of the sweep starts than the near end.
    pub span: Duration,
    /// One point's own fade, once its turn comes.
    pub fade: Duration,
}

impl Wave {
    /// That point's 0..=1 progress, `elapsed` seconds into the wave. Seconds rather than
    /// two `Instant`s because the callers evaluate one wave at many points of a frame (a
    /// mask's texels, a window of cards): plain `f32` throughout, no `Duration` arithmetic
    /// per point.
    pub fn frac_secs(self, elapsed: f32, progress: f32) -> f32 {
        let started = elapsed - self.span.as_secs_f32() * progress.clamp(0.0, 1.0);
        smoothstep((started / self.fade.as_secs_f32()).clamp(0.0, 1.0))
    }
}

/// Shared resolution for every diagonal dissolve mask (the launch backdrop's, the grid's):
/// tiny, since each is stretched over its own target and bilinear filtering turns it into a
/// continuous gradient.
pub const MASK_W: u32 = 64;
pub const MASK_H: u32 = 36;

/// Fills `buf` (resized to `MASK_W`x`MASK_H` RGBA8 if needed) with `wave`'s diagonal ramp,
/// `elapsed` seconds in: `rgb` in every texel's colour, `alpha_at(wave's 0..=1 frac at that
/// texel)` in its alpha. The wave runs on `(x + y)`, sweeping from the top-left corner.
///
/// Evaluated once per *diagonal* rather than once per texel — the frac is a function of
/// `x + y` alone, so a row of the mask is `MASK_W + MASK_H - 1` of them, not their product.
/// The buffer is kept across frames by its caller and only overwritten here, since this runs
/// every frame of a dissolve.
pub fn diagonal_mask(buf: &mut Vec<u8>, rgb: [u8; 3], wave: Wave, elapsed: f32, alpha_at: impl Fn(f32) -> f32) {
    let px = (MASK_W * MASK_H) as usize * 4;
    if buf.len() != px {
        buf.clear();
        buf.resize(px, 0);
    }
    let last = (MASK_W + MASK_H - 2) as f32;
    let mut diagonal = [0u8; (MASK_W + MASK_H - 1) as usize];
    for (d, a) in diagonal.iter_mut().enumerate() {
        *a = (255.0 * alpha_at(wave.frac_secs(elapsed, d as f32 / last))) as u8;
    }
    for y in 0..MASK_H {
        for x in 0..MASK_W {
            let i = ((y * MASK_W + x) * 4) as usize;
            buf[i] = rgb[0];
            buf[i + 1] = rgb[1];
            buf[i + 2] = rgb[2];
            buf[i + 3] = diagonal[(x + y) as usize];
        }
    }
}

/// Scales `base` by `1.0 + growth * frac` around its own center — the GPU
/// zoom-in technique behind every focus-pop in the app. The source tile is
/// rasterized once, at its literal size; only this destination rect changes
/// per frame, so the zoom costs nothing beyond a GPU texture copy at a
/// different size.
pub fn zoom_rect(base: Rect, frac: f32, growth: f32) -> Rect {
    scale_about(base, base, zoom_scale(frac, growth))
}

/// The factor [`zoom_rect`] scales by — for a piece composited onto a zooming tile,
/// which has to fold the same factor into its own transform.
pub fn zoom_scale(frac: f32, growth: f32) -> f32 {
    1.0 + growth * frac
}

/// Scale up from (1.0 - shrink) to full size. "Pop in" counterpart to `zoom_rect`.
pub fn pop_in_rect(base: Rect, frac: f32, shrink: f32) -> Rect {
    scale_about(base, base, pop_in_scale(frac, shrink))
}

/// The factor [`pop_in_rect`] scales by at `frac` — for a piece composited onto a
/// popping tile, which has to fold the same factor into its own transform.
pub fn pop_in_scale(frac: f32, shrink: f32) -> f32 {
    if frac >= 1.0 {
        1.0
    } else {
        1.0 - shrink * (1.0 - frac)
    }
}

/// Scales `rect` by `scale` about `pivot`'s center — for a sub-rect composited on top of
/// an already-scaled tile, which must ride that tile's transform. Passing the sub-rect as
/// its own pivot ([`zoom_rect`]) scales it in place; a piece sitting off-center (the grid
/// card's title strip, pinned to its bottom edge) needs the whole card as pivot or it
/// drifts away from the art beneath it.
pub fn scale_about(rect: Rect, pivot: Rect, scale: f32) -> Rect {
    let cx = pivot.x() as f32 + pivot.width() as f32 / 2.0;
    let cy = pivot.y() as f32 + pivot.height() as f32 / 2.0;
    let x = cx + (rect.x() as f32 - cx) * scale;
    let y = cy + (rect.y() as f32 - cy) * scale;
    Rect::new(
        x as i32,
        y as i32,
        (rect.width() as f32 * scale) as u32,
        (rect.height() as f32 * scale) as u32,
    )
}
