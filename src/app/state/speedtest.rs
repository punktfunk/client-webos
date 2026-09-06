//! The per-host network speed test — logic.
//!
//! Shape follows the pairing ceremony exactly (see `app::state::pairing`): the
//! measurement blocks for seconds, so it runs on a worker thread and reports back over a
//! channel drained each UI tick. Backing out drops the receiver, which orphans the
//! worker — its next send fails and it exits, tearing its own connection down.
//!
//! Measured throughput is end-to-end deliverable goodput (after AEAD decrypt), not pure
//! link speed. Bounds useful for bitrate picking on this TV.
//!
//! Rendering lives in `app::view::speedtest`.
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::model;
use crate::core::screen::Screen;

use crate::app::nav::ScreenKey;
use punktfunk_core::client::ProbeOutcome;

/// Fraction of the measured goodput to recommend as a bitrate, leaving headroom for
/// FEC overhead and real-world loss. Matches every other punktfunk client.
const RECOMMEND_NUMERATOR: u32 = 7;
const RECOMMEND_DENOMINATOR: u32 = 10;

/// Below this the measurement carried too little signal to recommend anything.
const MIN_USEFUL_KBPS: u32 = 2_000;

/// Where a running/finished speed test has got to.
pub(crate) enum SpeedTestState {
    Connecting,
    /// The burst is running; `partial` is the latest poll, if any has landed yet.
    Measuring {
        partial: Option<ProbeOutcome>,
    },
    /// `confirmed` is false if host's end-of-burst report didn't arrive.
    Done {
        outcome: ProbeOutcome,
        confirmed: bool,
    },
    Failed(String),
}

/// What the worker sends back.
pub(crate) enum SpeedTestMsg {
    Progress(ProbeOutcome),
    Done {
        outcome: Box<ProbeOutcome>,
        confirmed: bool,
    },
    Failed(String),
}

impl App {
    /// Opens `Screen::SpeedTest` for sidebar entry `idx` and starts the probe.
    pub(crate) fn open_speed_test(&mut self, idx: usize) {
        let Some(entry) = self.hosts.entries.get(idx) else {
            return;
        };
        let host = entry.host().to_string();
        let port = entry.port();
        let name = entry.name().to_string();
        // Only reachable for a paired host (see `App::host_menu_actions`), so the pin is
        // expected to be there; `None` still just falls back to TOFU rather than failing.
        let pin = self
            .hosts
            .known
            .iter()
            .find(|h| h.addr == host && h.port == port)
            .and_then(crate::core::model::KnownHost::fingerprint);

        self.screens.speed_test_name = name;
        self.screens.speed_test = Some(SpeedTestState::Connecting);
        self.nav.enter(Screen::SpeedTest, 0);
        tracing::info!("speed test: connecting to {host}:{port}");

        let identity = (self.identity.0.clone(), self.identity.1.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.speed_test = Some(rx);
        std::thread::spawn(move || {
            let progress_tx = tx.clone();
            let result = crate::session::probe::run_speed_probe(
                &host,
                port,
                identity,
                pin,
                crate::services::budget::SPEED_TEST,
                |partial| {
                    let _ = progress_tx.send(SpeedTestMsg::Progress(partial));
                },
            );
            let _ = match result {
                Ok(r) => tx.send(SpeedTestMsg::Done {
                    outcome: Box::new(r.outcome),
                    confirmed: r.confirmed,
                }),
                Err(e) => tx.send(SpeedTestMsg::Failed(crate::core::errors::friendly(&e))),
            };
        });
    }

    /// Drains the worker's updates, if any — called each tick alongside the other
    /// `drain_*`s. Returns whether anything changed.
    pub(crate) fn drain_speed_test(&mut self) -> bool {
        let Some(rx) = &self.jobs.speed_test else { return false };
        let mut changed = false;
        // WHY: keep only latest; burst between ticks costs one redraw, not per-message.
        while let Ok(msg) = rx.try_recv() {
            changed = true;
            match msg {
                SpeedTestMsg::Progress(p) => {
                    self.screens.speed_test = Some(SpeedTestState::Measuring { partial: Some(p) });
                }
                SpeedTestMsg::Done { outcome, confirmed } => {
                    tracing::info!(
                        "speed test: {} kbps, {:.1}% loss, {} bytes in {} ms (confirmed={confirmed})",
                        outcome.throughput_kbps,
                        outcome.loss_pct,
                        outcome.recv_bytes,
                        outcome.elapsed_ms
                    );
                    self.screens.speed_test = Some(SpeedTestState::Done {
                        outcome: *outcome,
                        confirmed,
                    });
                    self.nav.set_cursor(ScreenKey::SpeedTest, 0);
                    self.jobs.speed_test = None;
                    break;
                }
                SpeedTestMsg::Failed(e) => {
                    tracing::warn!("speed test failed: {e}");
                    self.screens.speed_test = Some(SpeedTestState::Failed(e));
                    self.jobs.speed_test = None;
                    break;
                }
            }
        }
        changed
    }

    pub(crate) fn handle_speed_test_event(&mut self, ev: MenuEvent) {
        let done = matches!(
            self.screens.speed_test,
            Some(SpeedTestState::Done { .. }) | Some(SpeedTestState::Failed(_))
        );
        match ev {
            // Back cancels (drops receiver → orphans worker → tears connection).
            MenuEvent::Back => self.close_speed_test(),
            _ if !done => {}
            MenuEvent::Left | MenuEvent::Right => {
                self.confirm_nav_event(ev);
            }
            MenuEvent::Confirm => {
                if self.nav.cursor(ScreenKey::SpeedTest) != 0 {
                    self.close_speed_test();
                    return;
                }
                let applied = match &self.screens.speed_test {
                    Some(SpeedTestState::Done { outcome, .. }) => recommended_kbps(outcome),
                    _ => None,
                };
                match applied {
                    Some(kbps) => {
                        self.settings_ui.settings.bitrate_kbps = kbps;
                        self.persist();
                        self.close_speed_test();
                    }
                    None => self.retry_speed_test(),
                }
            }
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary => {}
        }
    }

    /// Re-runs the probe against the host this screen was opened for. The host menu's
    /// index is still set (this screen is only ever reached from there), so nothing has
    /// to be stashed separately.
    pub(crate) fn retry_speed_test(&mut self) {
        let Some(idx) = self.screens.host_menu_index else {
            self.close_speed_test();
            return;
        };
        self.open_speed_test(idx);
    }

    /// Leaves the screen, abandoning any in-flight probe.
    pub(crate) fn close_speed_test(&mut self) {
        self.screens.speed_test = None;
        self.jobs.cancel_speed_test();
        self.back_to_host_menu();
    }
}

/// The bitrate to recommend from a finished measurement, in kbps — `None` when too little
/// got through to say anything useful. Clamped to the settings slider's own range, since
/// that's the only thing "Use this" can actually write.
pub(crate) fn recommended_kbps(outcome: &ProbeOutcome) -> Option<u32> {
    if outcome.throughput_kbps < MIN_USEFUL_KBPS {
        return None;
    }
    let raw = outcome.throughput_kbps / RECOMMEND_DENOMINATOR * RECOMMEND_NUMERATOR;
    // Whole Mbps, clamped to slider bounds (BITRATE_STEP_KBPS steps).
    let whole_mbps = (raw / 1000).max(1) * 1000;
    Some(whole_mbps.clamp(model::BITRATE_MIN_KBPS, model::BITRATE_MAX_KBPS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 70 % of the goodput in whole Mbps, inside the slider's range; nothing under 2 Mbps.
    #[test]
    fn the_recommendation_is_seventy_percent_clamped_to_the_slider() {
        let at = |kbps: u32| {
            recommended_kbps(&ProbeOutcome {
                throughput_kbps: kbps,
                ..Default::default()
            })
        };
        assert_eq!(at(1_999), None);
        assert_eq!(at(100_000), Some(70_000));
        assert_eq!(at(245_000), Some(171_000), "the G5's Wi-Fi ceiling");
        assert_eq!(at(400_000), Some(model::BITRATE_MAX_KBPS));
        assert_eq!(at(3_000), Some(model::BITRATE_MIN_KBPS));
    }
}
