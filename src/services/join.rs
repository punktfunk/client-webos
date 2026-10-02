//! Bounded thread joins.
//!
//! Teardown of anything holding the vendor video stack — a stream's pumps, the HDR calibration pattern feed — has the same problem: the thread re-checks its stop flag on a bounded
//! cadence, but the FFI call it is inside between checks has no timeout of its own. Lives here
//! rather than in `session` so `platform` can reach it too.

use std::time::Duration;

/// Ceiling on each teardown join. The FFI calls involved (NDL `play`/`play_audio`, and the
/// QUIC-close worker `NativeClient::drop` joins internally) have no timeout of their own — an
/// intermittently wedged vendor call must not freeze the whole app on the caller's thread.
/// Also the ceiling the stream teardown waits on (a different mechanism, same rationale).
pub const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Joins `handle` from a watcher thread so a hang inside it can't block the caller past
/// `timeout`. Returns `false` (and leaks the watcher, still waiting on the real join) if it
/// didn't finish in time.
///
/// On timeout, `on_wedged` returns a value the leaked watcher then holds until the real join lands
/// and drops on its way out — how the video/audio pumps keep NDL refused for exactly as long as a
/// wedged thread might still be inside it (`|| ndl::poison()`). The watcher already outlives the
/// timeout, so this needs no second thread; threads with nothing to hold pass `|| ()`. Not called
/// at all when the join lands in time.
pub fn join_with_timeout<T: Send + 'static, G: Send + 'static>(
    handle: std::thread::JoinHandle<T>,
    timeout: Duration,
    name: &str,
    on_wedged: impl FnOnce() -> G,
) -> bool {
    let (tx, rx) = std::sync::mpsc::channel();
    // The watcher blocks on this after joining, so the guard can only be dropped once the wedged
    // thread has actually returned — and the send below cannot race that, because the watcher is
    // not listening yet when the timeout fires.
    let (wedged_tx, wedged_rx) = std::sync::mpsc::channel::<G>();
    let spawned = std::thread::Builder::new()
        .name(format!("punktfunk-webos-join-{name}"))
        .spawn(move || {
            let _ = handle.join();
            let _ = tx.send(());
            // Either a guard to release (we were declared wedged) or a dropped sender (joined in
            // time, nothing was ever taken out).
            drop(wedged_rx.recv());
        });
    let Ok(watcher) = spawned else {
        // Can't even start the watcher (the process is out of threads, and `handle` went down
        // with the closure). Report clean: there is nothing left to wait on here, and blocking
        // teardown on a thread we can no longer join would be strictly worse.
        return true;
    };
    if rx.recv_timeout(timeout).is_ok() {
        // BEFORE the join: the watcher is parked on `wedged_rx.recv()` and only this sender going
        // away releases it. Joining first would deadlock the teardown on a thread that finished.
        drop(wedged_tx);
        let _ = watcher.join();
        true
    } else {
        tracing::error!(
            "{name} thread did not finish within {timeout:?} — leaking it \
             (likely a wedged NDL/FFI or QUIC-close call)"
        );
        // Unbounded channel: never blocks, and the value stays queued for however long the wedged
        // thread takes. A failed send would mean the watcher died with the guard, which drops it —
        // correct either way, since a dead watcher means the join already returned.
        let _ = wedged_tx.send(on_wedged());
        false
    }
}
