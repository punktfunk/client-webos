//! The connect-path time budgets, in one place so "how long do we wait for a host" doesn't mean
//! two different things depending on which screen is in front of it.
use std::time::Duration;

/// One reach attempt against a host — a handshake, the reachability dot, the wake flow's
/// "is it back yet?" probe — all asking "is it up *now*". On a LAN an up host answers well
/// inside this; an off one fails fast. Also the per-request TCP connect budget.
pub const PROBE: Duration = Duration::from_secs(3);

/// A stream's handshake. The host brings up its capture and encoder before it answers, and a
/// gamescope host does that behind any other client's session. The other clients wait 15 s too.
pub const CONNECT: Duration = Duration::from_secs(15);

/// A host that answered but isn't ready to stream yet: an unpinned connection waiting on the
/// host's operator to approve this client, and a PIN handshake waiting on someone to walk to
/// their PC. A shorter budget sent the user
/// back to the menu with "couldn't connect" against a host that was merely still starting.
pub const HOST_WAIT: Duration = Duration::from_secs(185);

/// The exit action's whole budget — connect, mTLS handshake and POST together.
///
/// Brutally short because the user is watching the app close and nothing on screen is waiting
/// for the answer: a host that cannot be told inside this stays up, which is the same outcome
/// as not asking. Affordable only because the exit action is now skipped entirely unless the
/// host answered its last reachability check, so this is a warm LAN peer or nothing.
pub const EXIT_ACTION: Duration = Duration::from_millis(200);

/// One host request that should already have an answer: a library listing, a `/launch`, a
/// `/serverinfo`. Not a wait for the host to become ready — that is [`HOST_WAIT`], spent by re-trying
/// requests with this budget rather than by stretching one of them.
pub const REQUEST: Duration = PROBE.saturating_mul(3);

/// Connect budget for the speed test's throwaway session. Longer than [`PROBE`] because the
/// host brings up a real encode session for it, and the user opened this screen expecting to wait —
/// but not [`HOST_WAIT`]: a host that needs minutes here has already answered the question.
pub const SPEED_TEST: Duration = Duration::from_secs(20);
