//! What the stream draws over the video: the stats card, the log tail, the toast, the exit hint
//! and the dial, all on one transparent pass over NDL's punch-through plane.
//!
//! The window is never shown/hidden (that crashed an earlier attempt, see docs/NOTES.md), so
//! these draw onto the transparent stream window via per-pixel alpha, and the last one leaving
//! has to wipe what it left behind.

use super::*;
use punktfunk_core::hud::{self as core_hud, Extra, HudCorner, HudLine, Role, StatsSnapshot, StatsVerbosity};

/// One frame of the dial's animation. The loop runs every 2 ms; the ring needs no more than 60 Hz.
const RING_FRAME: Duration = Duration::from_millis(16);
/// How long a freeze-until-reanchor hold must last before the toast names it. An RFI recovery
/// lifts a hold within a round trip and the startup capacity probe's own loss clears at the
/// burst's end; a toast for those flashed on every blip and said nothing the picture did not.
const HOLD_TOAST_AFTER: Duration = Duration::from_millis(300);
/// The exit hint's time on screen before it fades, and the fade: six seconds in all.
const EXIT_HINT_HOLD: Duration = Duration::from_millis(5_400);
const EXIT_HINT_FADE: Duration = Duration::from_millis(600);

/// The overlays' state across ticks.
pub(super) struct Hud {
    /// The stats card's tier. Green cycles it, session-only.
    tier: StatsVerbosity,
    /// Whether the card is wanted. The pumps read the diagnostics flag too — with nothing on
    /// glass to show them, the video thread skips every counter the overlay is the only reader
    /// of — and that flag is DERIVED from [`Self::fade`] each frame rather than set alongside
    /// this, so there is one writer and no second copy of the state to keep in step.
    enabled: bool,
    advanced: bool,
    /// Top right until the player picks a corner; the size multiplies the display's.
    corner: HudCorner,
    scale: f32,
    /// Fades in/out on the same curve as the toast — see `ModalFade::visibility_alpha`.
    fade: overlay::fade::ModalFade<()>,
    /// The stats card's lines, rebuilt from the core's window once a second (and on a tier
    /// change), and drawn as they stand between.
    lines: Vec<HudLine>,
    snap: Option<StatsSnapshot>,
    built_at: Option<Instant>,
    /// CPU ticks at the last snapshot, for the card's CPU line.
    prev_cpu: Option<(u64, Instant)>,
    /// Transient toasts.
    notif: overlay::Notification,
    /// When the one-line exit hint went up, if Settings asks for it.
    exit_hint_at: Option<Instant>,
    /// When the current freeze-until-reanchor hold began (`stats.holding`, see `session::pump`),
    /// and whether it has been announced — see [`HOLD_TOAST_AFTER`].
    hold_since: Option<Instant>,
    hold_toasted: bool,
    /// Catches the last overlay's fade-out edge so the canvas gets wiped once; the stats and
    /// log cards redraw at their own slower cadence. Also set by a wipe that could not draw.
    was_active: bool,
    last: Option<Instant>,
    /// One line per streak of undrawable frames — see `overlay::drawn`.
    warned: bool,
    ring_drawn: u64,
    ring_drawn_at: Instant,
}

impl Hud {
    /// The overlays a stream starts with. `initial_wipe` is the stream's first clear, which
    /// stays owed when it could not draw.
    pub(super) fn new(settings: &store::Settings, connected: &session::Connected, initial_wipe: Result<()>) -> Self {
        let tier = settings.stats_verbosity();
        let enabled = tier != StatsVerbosity::Off;
        // The session starts where the setting says; the dial flips it from there.
        connected.client.set_invert_scroll(settings.invert_scroll);
        connected.stats().set_diagnostics(enabled);
        connected.set_hud_enabled(enabled);
        let mut fade = overlay::fade::ModalFade::<()>::overlay();
        if enabled {
            fade.open();
        }
        let mut warned = false;
        let was_active = !overlay::drawn(initial_wipe, &mut warned);
        Self {
            tier,
            enabled,
            advanced: settings.advanced_stats,
            corner: settings.hud_corner(HudCorner::TopRight),
            scale: core_hud::stats_scale(settings.stats_scale_pct),
            fade,
            lines: Vec::new(),
            snap: None,
            built_at: None,
            prev_cpu: None,
            notif: overlay::Notification::new(),
            // The one-line exit hint, from the first frame of the stream.
            exit_hint_at: settings.exit_hint.then(Instant::now),
            hold_since: None,
            hold_toasted: false,
            was_active,
            last: None,
            warned,
            ring_drawn: 0,
            ring_drawn_at: Instant::now(),
        }
    }

    pub(super) fn tier(&self) -> StatsVerbosity {
        self.tier
    }

    /// The next stats tier (green, or the dial).
    pub(super) fn cycle_stats(&mut self) {
        self.tier = self.tier.next();
        let was_enabled = self.enabled;
        self.enabled = self.tier != StatsVerbosity::Off;
        self.redraw_now();
        if self.enabled && !was_enabled {
            self.fade.reopen();
        } else if !self.enabled {
            self.fade.close(());
        }
        // A fading-out card keeps its last lines; a visible one re-renders at once.
        if let (true, Some(snap)) = (self.enabled, &self.snap) {
            self.lines = core_hud::format(snap, self.tier, self.advanced);
        }
    }

    /// Draw on the next tick rather than at the cadence.
    pub(super) fn redraw_now(&mut self) {
        self.last = None;
    }

    /// Connection-issue toast, once per hold that outlasts [`HOLD_TOAST_AFTER`] — the same
    /// "network trouble" signal the stats overlay's "Beat" line reads, visible without the
    /// overlay open. No matching "recovered" toast: the picture resuming is that signal.
    pub(super) fn note_hold(&mut self, holding: bool) {
        if holding {
            let since = *self.hold_since.get_or_insert_with(Instant::now);
            if !self.hold_toasted && since.elapsed() >= HOLD_TOAST_AFTER {
                self.hold_toasted = true;
                tracing::warn!("connection issues detected (freeze-until-reanchor)");
                self.notif.show("Connection issues — recovering...");
                self.redraw_now();
            }
        } else {
            self.hold_since = None;
            self.hold_toasted = false;
        }
    }

    /// Whether a frame drew — see `overlay::drawn`.
    pub(super) fn drawn(&mut self, result: Result<()>) -> bool {
        overlay::drawn(result, &mut self.warned)
    }

    /// Clears the window, so a frame that is done does not stick over the video.
    pub(super) fn wipe(&mut self, cx: &mut Cx<'_>) -> bool {
        let wipe = overlay::wipe(cx.gl, cx.canvas, cx.fonts);
        self.drawn(wipe)
    }

    /// The overlays' pass, while the stop dialog has no frame of its own on screen
    /// (`dialog_shown`). Stats and log share one clear/execute/present so neither erases the
    /// other's tile.
    pub(super) fn draw(&mut self, cx: &mut Cx<'_>, ring: &mut pf_console_ui::Ring, dialog_shown: bool) {
        // `log_overlay_lines()` deferred to the throttled block below, not called every
        // ~2ms tick — it locks the same mutex log writes contend on ~500x/s.
        let notif_frame = if dialog_shown {
            None
        } else {
            self.notif.frame().map(|(t, a)| (t.to_string(), a))
        };
        let notif_active = notif_frame.is_some();
        let hint_frame = self
            .exit_hint_at
            .filter(|_| !dialog_shown)
            .and_then(|at| overlay::fade::hold_alpha(at, EXIT_HINT_HOLD, EXIT_HINT_FADE));
        // Fade in/out on the toast's curve instead of cutting instantly; `visibility_alpha`
        // keeps returning `Some` through the close fade after the toggle itself flips off.
        let stats_alpha = self.fade.visibility_alpha(self.enabled);
        // The counters follow what is VISIBLE, fade included — stopping them at the toggle
        // freezes the figures for the last frames of the fade-out.
        cx.connected.stats().set_diagnostics(stats_alpha.is_some());
        cx.connected.set_hud_enabled(stats_alpha.is_some());
        let log_overlay_on = log_overlay_state() != LogOverlayState::Off;
        let ring_damage = ring.damage();
        let ring_visible = ring_damage != 0;
        let active = stats_alpha.is_some() || log_overlay_on || notif_active || ring_visible || hint_frame.is_some();
        if self.was_active && !active {
            // Nothing else clears this window when the last overlay disappears.
            // A wipe that could not draw stays owed, so the next tick tries it again.
            self.was_active = !self.wipe(cx);
        } else {
            self.was_active = active;
        }
        // A fade in flight needs frequent frames; steady-state stats/log are fine at ~2Hz.
        let fading = notif_active || hint_frame.is_some() || self.fade.is_animating();
        let redraw_interval = if fading {
            Duration::from_millis(33)
        } else {
            Duration::from_millis(500)
        };
        let ring_due = ring_visible && ring_damage != self.ring_drawn && self.ring_drawn_at.elapsed() >= RING_FRAME;
        if !(active && !dialog_shown && (ring_due || self.last.is_none_or(|t| t.elapsed() >= redraw_interval))) {
            return;
        }
        self.last = Some(Instant::now());
        let ring_dt = self.ring_drawn_at.elapsed().as_secs_f64();
        self.ring_drawn_at = Instant::now();
        self.ring_drawn = ring_damage;
        // The core window closes once a second: every rate it reports is per window.
        if self.enabled && self.built_at.is_none_or(|t| t.elapsed() >= Duration::from_secs(1)) {
            self.built_at = Some(Instant::now());
            let mut snap = cx.connected.hud_snapshot();
            snap.extras = tv_extras(cx.connected, &mut self.prev_cpu);
            self.lines = core_hud::format(&snap, self.tier, self.advanced);
            self.snap = Some(snap);
        }
        let log_lines = log_overlay_lines();
        let pad = !cx.pads.is_empty();
        let (lines, tier, corner, scale) = (&self.lines, self.tier, self.corner, self.scale);
        let frame = overlay::frame(cx.gl, cx.canvas, cx.fonts, cx.display, overlay::TRANSPARENT, |f| {
            if let Some(alpha) = stats_alpha {
                overlay::stats(f, lines, stats_hint(tier), alpha, corner, scale);
            }
            if let Some(lines) = &log_lines {
                overlay::log(f, lines);
            }
            if let Some((text, alpha)) = &notif_frame {
                overlay::toast(f, text, *alpha);
            }
            if let Some(alpha) = hint_frame {
                overlay::exit_hint(f, exit_hint_text(pad), alpha);
            }
            if ring_visible {
                ring.render(f.canvas, f.w as u32, f.h as u32, f.k, f.fonts, ring_dt);
            }
        });
        self.drawn(frame);
    }
}

/// How to leave with the input in hand: the pad chord when a controller is attached.
fn exit_hint_text(pad: bool) -> &'static str {
    if pad {
        "Hold L1 + R1 + Start + Select to leave"
    } else {
        "Hold Back to leave"
    }
}

/// What the green button does next: more detail, or hide from the top tier.
fn stats_hint(tier: StatsVerbosity) -> &'static str {
    match tier {
        StatsVerbosity::Detailed | StatsVerbosity::Off => "Press green to hide this overlay",
        StatsVerbosity::Compact | StatsVerbosity::Normal => "Press green for more detail",
    }
}

/// Lines only this client measures: NDL's feed, backlog and hold, the audio route, the pacing
/// loop, and this process's CPU and memory. `prev_cpu` spans CPU ticks between two calls.
fn tv_extras(connected: &session::Connected, prev_cpu: &mut Option<(u64, Instant)>) -> Vec<Extra> {
    let stats = connected.stats();
    let backlog = stats.render_backlog.load(Ordering::Relaxed);
    let mut ndl = format!(
        "NDL feed {:.1} ms · backlog {}",
        stats.feed_us.load(Ordering::Relaxed) as f32 / 1000.0,
        if backlog < 0 {
            "n/a".to_string()
        } else {
            backlog.to_string()
        },
    );
    if stats.holding.load(Ordering::Relaxed) {
        ndl.push_str(" · holding");
    }
    // NDL paces the picture on the plane's lead, so a lead sagging towards zero reads as stutter.
    let layout = connected.audio_layout();
    let audio = if connected.audio_route.on_ndl_plane() {
        format!(
            "{} {layout} · NDL · lead {} ms · av stamp {:+} ms",
            connected.audio_route.overlay_tag(),
            stats.audio_plane_lead_ms.load(Ordering::Relaxed),
            stats.av_offset_ms.load(Ordering::Relaxed),
        )
    } else {
        format!(
            "{} {layout} · buf {} ms",
            connected.audio_route.overlay_tag(),
            connected.audio_buffer_ms()
        )
    };
    // `cushion` leads: it is the only figure here that answers "is the presentation setting doing
    // anything". Jitter is the measured residual, independent of the cushion by construction, and
    // `late` is cumulative — neither moves when the setting does.
    let slack = stats.pacing_min_slack_us.load(Ordering::Relaxed);
    let pacing = format!(
        "pace cushion {:.1} ms · jitter {:.1} ms · late {}{}",
        stats.pacing_cushion_us.load(Ordering::Relaxed) as f32 / 1000.0,
        stats.pacing_jitter_us.load(Ordering::Relaxed) as f32 / 1000.0,
        stats.pacing_late.load(Ordering::Relaxed),
        // Worst complete-AU margin of the last window. Negative while jitter reads healthy is a
        // large frame finishing against the deadline its first piece set.
        if slack == i32::MIN {
            String::new()
        } else {
            format!(" · slack {:+.1} ms", slack as f32 / 1000.0)
        },
    );
    let mut out = vec![Extra::detail(ndl), Extra::detail(audio), Extra::detail(pacing)];
    // The set's own headroom, in both vocabularies. CPU shows from the second sample on.
    if let Some((ticks, mem_bytes)) = crate::platform::webos::device::process_cpu_mem() {
        let cpu = prev_cpu.map(|(prev, at)| {
            let secs = at.elapsed().as_secs_f64().max(0.001);
            let pct =
                ticks.saturating_sub(prev) as f64 / crate::platform::webos::device::clock_ticks_per_sec() as f64 / secs
                    * 100.0;
            format!("CPU {pct:.0}% · ")
        });
        *prev_cpu = Some((ticks, Instant::now()));
        out.push(Extra {
            text: format!(
                "{}RAM {:.0} MB",
                cpu.unwrap_or_default(),
                mem_bytes as f64 / (1024.0 * 1024.0)
            ),
            tier: StatsVerbosity::Detailed,
            advanced_only: false,
            role: Role::Muted,
        });
    }
    out
}
