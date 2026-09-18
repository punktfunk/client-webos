//! Coalesce backdrop changes without freezing a continuously changing page.

use std::time::{Duration, Instant};

const REFRESH_INTERVAL: Duration = Duration::from_millis(100);
const ANIMATING_INTERVAL: Duration = Duration::from_millis(150);

#[derive(Default)]
pub(crate) struct BackdropRefresh {
    dirty: bool,
    attempted: Option<Instant>,
}

impl BackdropRefresh {
    pub(crate) fn invalidate(&mut self) {
        self.dirty = true;
    }

    pub(crate) fn due(&self, now: Instant, animating: bool) -> bool {
        let interval = if animating {
            ANIMATING_INTERVAL
        } else {
            REFRESH_INTERVAL
        };
        self.dirty && self.attempted.is_none_or(|last| now.duration_since(last) >= interval)
    }

    pub(crate) fn complete(&mut self, now: Instant, success: bool) {
        self.attempted = Some(now);
        self.dirty = !success;
    }
}
