//! The per-host network speed test — presentation. Logic lives in `app::state::speedtest`.
use crate::app::state::speedtest::{recommended_kbps, SpeedTestState};

pub(crate) const TITLE: &str = "Network speed test";

/// Primary button label (built from the measurement, not constant). No recommendation →
/// "Retry" rather than "Close", to leave the user an action on a low-throughput result.
pub(crate) fn apply_label(recommended: Option<u32>) -> String {
    recommended.map_or_else(|| "Retry".to_string(), |kbps| format!("Use {} Mbps", kbps / 1000))
}

/// Whether the test has stopped — the state in which the card grows a button row.
pub(crate) fn finished(state: Option<&SpeedTestState>) -> bool {
    matches!(
        state,
        Some(SpeedTestState::Done { .. }) | Some(SpeedTestState::Failed(_))
    )
}

/// The status sentence for the current phase — also measured (without drawing) to place
/// the card and its buttons, so it lives in one place.
pub(crate) fn status(state: Option<&SpeedTestState>, host_name: &str) -> String {
    match state {
        None | Some(SpeedTestState::Connecting) => format!("Connecting to {host_name}…"),
        Some(SpeedTestState::Measuring { partial }) => {
            // Deliberately bytes, not Mbps: `throughput_kbps`'s denominator (since core
            // 0.24, the client-measured receive interval, falling back to the host's
            // burst duration) is frozen only when the end-of-burst report lands — so a
            // "Mbps so far" reading here could never show anything. `recv_bytes` is live
            // throughout.
            let so_far = partial
                .filter(|p| p.recv_bytes > 0)
                .map_or_else(String::new, |p| format!(" — {} MB in", p.recv_bytes / (1024 * 1024)));
            format!("Measuring{so_far} over the real data plane…")
        }
        Some(SpeedTestState::Done { outcome, confirmed }) => {
            // The burst deliberately asks for more than the link can carry — that
            // overshoot is *how* the ceiling is found — so a high loss figure here is
            // expected and says nothing bad about the network on its own. Labelled
            // accordingly, since a bare "80% loss" reads as a fault.
            let detail = if *confirmed {
                format!(
                    "({:.0}% of the deliberately over-capacity test burst didn't fit — \
                     that's how the ceiling is found)",
                    outcome.loss_pct
                )
            } else {
                "(the host's own report didn't make it back, so this is measured from what \
                 arrived here — treat it as a floor)"
                    .to_string()
            };
            let base = format!(
                "{} Mbps delivered · {} MB in {} ms\n{detail}",
                outcome.throughput_kbps / 1000,
                outcome.recv_bytes / (1024 * 1024),
                outcome.elapsed_ms,
            );
            match recommended_kbps(outcome) {
                Some(kbps) => format!(
                    "{base}\n\nRecommended bitrate {} Mbps (~70% of measured, leaving headroom \
                     for FEC and loss). This measures what this TV can actually receive and \
                     decrypt, not raw link speed.",
                    kbps / 1000
                ),
                None => format!(
                    "{base}\n\nToo little got through to recommend a bitrate. If the host \
                     reported bytes sent but none arrived, the path is dropping them; if it \
                     sent none at all, it may not support the probe. The app log has both \
                     figures."
                ),
            }
        }
        Some(SpeedTestState::Failed(e)) => format!("Couldn't measure: {e}"),
    }
}

/// The recommendation this result's primary button would apply, if any.
pub(crate) fn recommendation(state: Option<&SpeedTestState>) -> Option<u32> {
    match state {
        Some(SpeedTestState::Done { outcome, .. }) => recommended_kbps(outcome),
        _ => None,
    }
}
