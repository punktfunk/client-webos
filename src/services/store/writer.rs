//! Off-thread, coalescing writer for the persisted document.
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{save, Persisted};

#[derive(Default)]
struct Queue {
    /// The snapshot not yet written, emptied by the worker as it picks it up.
    pending: Option<Persisted>,
    /// The last snapshot queued. Outlives `pending`, since the unchanged-snapshot comparison has
    /// to keep working after the worker has drained it.
    last: Persisted,
    /// Consecutive failed attempts at `last`; nonzero means what is on disk is older than it.
    failures: u8,
    retry_at: Option<Instant>,
    stop: bool,
}

/// Persists the document on a dedicated background thread instead of the caller's —
/// [`save`]'s write-then-rename blocks on real disk I/O (measured ~100-200ms on-device), which is
/// fine for the occasional save but was stalling the UI thread on every single settings-row
/// adjustment (bitrate slider steps, a toggle flip), reading as input lag on the very controls
/// someone expects to feel instant.
///
/// A single long-lived writer thread, not one spawn per save: rapid adjustments (holding the
/// bitrate slider) replace the pending value rather than queuing every intermediate one, so a
/// burst of changes costs one disk write of the final state, not N — and, since one thread ever
/// calls [`save`], writes can't complete out of order the way N independently-spawned threads
/// racing the filesystem could.
///
/// It carries the whole document, so a host edit and a settings change can't race into disagreeing
/// files — whichever snapshot lands last came from the same in-memory state.
///
/// **Unchanged snapshots never reach the disk.** Callers hand over the whole document, and several
/// fire on events that usually change nothing (an mDNS reply repeating a known MAC, re-selecting
/// the active host, leaving Settings untouched). With [`StateWriter::spawn`]'s baseline being what
/// was just loaded, an unchanged launch writes zero times: durable state, not a scratch file.
pub struct StateWriter {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    /// `None` only after `Drop` has taken and joined it.
    thread: Option<JoinHandle<()>>,
}

/// Poisoning means the worker panicked inside `save`, which nothing here can recover from.
const POISONED: &str = "state-writer mutex poisoned";
const SAVE_ATTEMPTS: u8 = 3;
const RETRY_DELAY: Duration = Duration::from_millis(250);

impl StateWriter {
    /// `baseline` is the document as loaded from disk, so a save matching it is a no-op.
    pub fn spawn(baseline: Persisted) -> Self {
        let queue = Arc::new((
            Mutex::new(Queue {
                last: baseline,
                ..Queue::default()
            }),
            Condvar::new(),
        ));
        let worker = queue.clone();
        let thread = std::thread::spawn(move || {
            let (lock, cvar) = &*worker;
            let mut guard = lock.lock().expect(POISONED);
            loop {
                if !guard.stop {
                    if let Some(wait) = guard.retry_at.and_then(|at| at.checked_duration_since(Instant::now())) {
                        guard = cvar.wait_timeout(guard, wait).expect(POISONED).0;
                        continue;
                    }
                }
                guard.retry_at = None;
                match guard.pending.take() {
                    Some(state) => {
                        // Unlocked across the write so `save` below never blocks on disk I/O.
                        drop(guard);
                        let result = save(&state);
                        if let Err(e) = &result {
                            tracing::warn!("settings write failed: {e:#}");
                        }
                        guard = lock.lock().expect(POISONED);
                        // A failed old snapshot must never replace a newer queued document.
                        if guard.last != state {
                            continue;
                        }
                        if result.is_err() {
                            guard.failures = guard.failures.saturating_add(1);
                            if !guard.stop && guard.failures < SAVE_ATTEMPTS {
                                guard.pending = Some(state);
                                guard.retry_at = Some(Instant::now() + RETRY_DELAY * u32::from(guard.failures));
                            }
                        } else {
                            guard.failures = 0;
                        }
                    }
                    // Stopping only once nothing is pending, so the last snapshot still lands.
                    None if guard.stop => return,
                    None => guard = cvar.wait(guard).expect(POISONED),
                }
            }
        });
        Self {
            queue,
            thread: Some(thread),
        }
    }

    /// Queues `state`, replacing any snapshot not yet written and dropping one equal to the last
    /// queued. Returns immediately — never writes on the calling thread.
    pub fn save(&self, state: Persisted) {
        let (lock, cvar) = &*self.queue;
        let mut queue = lock.lock().expect(POISONED);
        if queue.last == state && (queue.failures == 0 || queue.pending.is_some()) {
            return;
        }
        queue.last.clone_from(&state);
        queue.pending = Some(state);
        queue.failures = 0;
        queue.retry_at = None;
        drop(queue);
        cvar.notify_one();
    }
}

impl Drop for StateWriter {
    /// Wakes the worker with `stop` set so it exits after flushing any pending save, then joins it
    /// — otherwise every menu re-entry (a fresh `App`, a fresh `StateWriter`) leaked one thread
    /// parked forever on the `Condvar`.
    fn drop(&mut self) {
        let (lock, cvar) = &*self.queue;
        lock.lock().expect(POISONED).stop = true;
        cvar.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
