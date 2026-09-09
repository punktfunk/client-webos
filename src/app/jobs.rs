//! Every background job the menu owns, in one place.
//!
//! Each field is the receiving end of work running off the UI thread; `None` means nothing is in
//! flight. Cancellation is dropping the receiver, so the cancel helpers here are the whole
//! protocol — the sender notices on its next send and stops.
//!
//! The `drain_*` bodies stay on `App` (they apply results to app state, which is where that
//! belongs); `App::drain_jobs` is the one call the runtime makes per tick.

use std::sync::mpsc::Receiver;

use crate::app::state::{reach::Reachability, sendlogs::SendLogsMsg, speedtest::SpeedTestMsg};
use crate::app::PairingOutcome;
use crate::services::art::ArtLoader;
use crate::services::discovery::Discovery;
use crate::services::library::{GamesLoaded, LibraryError};
use crate::services::power::PowerRights;
use crate::services::status::RunningLoaded;
use crate::services::store::ExitAction;

#[derive(Default)]
pub(crate) struct Jobs {
    /// `None` if the mDNS daemon didn't start. Owned here so it stops with the menu.
    pub(crate) discovery: Option<Discovery>,
    pub(crate) games: Option<Receiver<GamesLoaded>>,
    /// Answers [`App::tick_running`].
    pub(crate) running: Option<Receiver<RunningLoaded>>,
    pub(crate) art: Option<ArtLoader>,
    /// Drained each tick by `drain_pairing`; dropping it (Back while busy) cancels.
    pub(crate) pairing: Option<Receiver<PairingOutcome>>,
    pub(crate) reach: Option<Receiver<Reachability>>,
    /// Delivers the background probe's progress/result — dropping it cancels.
    pub(crate) speed_test: Option<Receiver<SpeedTestMsg>>,
    /// Delivers the background log upload's result; `None` when no upload is in flight.
    pub(crate) send_logs: Option<Receiver<SendLogsMsg>>,
    /// Answers [`App::start_root_probe`].
    pub(crate) rooted: Option<Receiver<bool>>,
    /// Answers [`App::start_power_probe`] — whether this pairing may drive the host's power.
    pub(crate) power_access: Option<PowerProbeJob>,
    /// Answers the host menu's power row — a sleep/shutdown the user asked for by hand.
    pub(crate) power_action: Option<PowerActionJob>,
    /// Whether the Experimental screen still owes its root probe. The probe forks
    /// `luna-send-pub` and wakes the Homebrew Channel's service, and on this hardware that
    /// costs enough CPU to drop frames out of whatever is animating — so it is held until the
    /// modal it belongs to has finished opening rather than started on the open frame.
    pub(crate) root_probe_owed: bool,
}

/// One rights probe in flight. The target rides along for the same reason
/// [`PowerActionJob`]'s does: the answer may land after the screen that asked has moved to
/// another host, and the reachability it implies belongs to the host that was asked.
pub(crate) struct PowerProbeJob {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) rx: Receiver<Result<PowerRights, LibraryError>>,
}

/// One hand-invoked power action in flight. The target and the action id ride along because
/// only the host's reply is on the channel, while the menu that started it is already closed —
/// so neither the sentence that reports it nor the reachability it implies can be looked up.
pub(crate) struct PowerActionJob {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) action: ExitAction,
    pub(crate) rx: Receiver<Result<(), LibraryError>>,
}

impl Jobs {
    pub(crate) fn cancel_pairing(&mut self) {
        self.pairing = None;
    }

    pub(crate) fn cancel_speed_test(&mut self) {
        self.speed_test = None;
    }

    /// Drops the library fetch and the art loader together — they are one pipeline, and a stale
    /// fetch landing after a host switch would start art for the wrong library.
    pub(crate) fn cancel_library(&mut self) {
        self.games = None;
        self.art = None;
        self.running = None;
    }
}

impl crate::app::App {
    /// One tick's worth of background results, applied in the order the runtime used to call
    /// them in. Returns whether anything changed and so a redraw is owed.
    pub fn drain_jobs(&mut self) -> bool {
        self.tick_root_probe();
        let mut dirty = self.drain_discovery();
        dirty |= self.drain_art();
        dirty |= self.drain_games();
        dirty |= self.drain_running();
        dirty |= self.drain_pairing();
        dirty |= self.drain_rooted();
        dirty |= self.drain_power_access();
        dirty |= self.drain_power_action();
        dirty |= self.drain_speed_test();
        dirty |= self.drain_send_logs();
        self.tick_reachability();
        dirty |= self.drain_reachability();
        dirty
    }
}
