//! What the selected host has launched right now — the grid's running dot. Pure logic; the
//! dot itself is painted by `app::draw::home::poster`.
//!
//! Polled rather than pushed: the management API has no event lane this client speaks, so the
//! answer is a `GET /api/v1/status` on the same mTLS agent the library came over.
use std::time::{Duration, Instant};

use crate::app::App;
use crate::core::screen::Screen;

/// How often the selected host is re-asked. The desktop's own rate limit for this fact, and
/// the reason it is a limit rather than a cadence: a game the user launched from this TV lights
/// its dot up to this long late, which is the price of not holding an mTLS round-trip open
/// against the host every second the Home screen is up.
const RUNNING_INTERVAL: Duration = Duration::from_secs(20);

impl App {
    /// Asks the selected host what it has running, if that is due and nothing is in flight.
    ///
    /// Only from Home: the dot is only ever drawn there, and a poll behind a modal or mid-stream
    /// would be a request nothing reads. A host switch clears the stamp with the rest of the
    /// library, so picking a host asks immediately rather than waiting out the interval.
    pub(crate) fn tick_running(&mut self) {
        if self.jobs.running.is_some() || self.nav.screen != Screen::Home {
            return;
        }
        if self
            .library
            .running_last
            .is_some_and(|t| t.elapsed() < RUNNING_INTERVAL)
        {
            return;
        }
        let Some((host, port)) = self.library.selected_host.clone() else {
            return;
        };
        // Stamping here delays first poll by 20s; wait for library load.
        if self.library.games.is_empty() {
            return;
        }
        let known = self.hosts.known.iter().find(|h| h.addr == host && h.port == port);
        let mgmt_port = known
            .and_then(|h| h.mgmt_port)
            .unwrap_or(crate::services::library::DEFAULT_MGMT_PORT);
        let fingerprint = known.and_then(crate::core::model::KnownHost::fingerprint);
        let identity = (self.identity.0.clone(), self.identity.1.clone());
        self.library.running_last = Some(Instant::now());
        self.jobs.running = Some(crate::services::status::load_running_async(
            host,
            port,
            mgmt_port,
            identity,
            fingerprint,
            crate::services::budget::REQUEST,
        ));
    }

    /// Takes the poll's answer. Returns whether the set changed — an unchanged answer is the
    /// common case (a host runs the same game for hours) and must not cost a redraw.
    pub(crate) fn drain_running(&mut self) -> bool {
        let Some(rx) = &self.jobs.running else { return false };
        let Ok(loaded) = rx.try_recv() else { return false };
        self.jobs.running = None;
        // Poll may outlive host switch; discard if not from current host.
        if self.library.selected_host.as_ref() != Some(&(loaded.host, loaded.port)) {
            return false;
        }
        let running: std::collections::HashSet<String> = loaded.running.into_iter().collect();
        if running == self.library.running {
            return false;
        }
        tracing::debug!("running: {} title(s) up on the selected host", running.len());
        self.library.running = running;
        true
    }
}
