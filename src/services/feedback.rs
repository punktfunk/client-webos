//! Latest-state delivery; deadlines independent of events.
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub enum Received<T> {
    State(T),
    Deadline,
    Closed,
}

pub struct Mailbox<T> {
    state: Mutex<(Option<T>, bool)>,
    wake: Condvar,
}

impl<T> Mailbox<T> {
    pub fn new() -> Self {
        Self {
            state: Mutex::new((None, false)),
            wake: Condvar::new(),
        }
    }

    pub fn replace(&self, value: T) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.1 {
            state.0 = Some(value);
            self.wake.notify_one();
        }
    }

    /// Sealed state; immune to `replace()`.
    pub fn finish(&self, value: T) {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.1 {
            state.0 = Some(value);
            state.1 = true;
            self.wake.notify_one();
        }
    }

    pub fn close(&self) {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner).1 = true;
        self.wake.notify_one();
    }

    pub fn receive(&self, deadline: Option<Instant>) -> Received<T> {
        let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            if let Some(value) = state.0.take() {
                return Received::State(value);
            }
            if state.1 {
                return Received::Closed;
            }
            state = if let Some(deadline) = deadline {
                let wait = deadline.saturating_duration_since(Instant::now());
                if wait.is_zero() {
                    return Received::Deadline;
                }
                self.wake
                    .wait_timeout(state, wait)
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .0
            } else {
                self.wake.wait(state).unwrap_or_else(std::sync::PoisonError::into_inner)
            };
        }
    }
}

pub struct Pending<T> {
    value: Option<T>,
    sent: Option<T>,
    next_send: Option<Instant>,
    interval: Duration,
}

impl<T: Copy + PartialEq> Pending<T> {
    pub fn new(interval: Duration) -> Self {
        Self {
            value: None,
            sent: None,
            next_send: None,
            interval,
        }
    }

    pub fn offer(&mut self, value: T) {
        self.value = (self.sent != Some(value)).then_some(value);
    }

    pub fn deadline(&self, now: Instant) -> Option<Instant> {
        self.value.map(|_| self.next_send.unwrap_or(now))
    }

    pub fn take_due(&mut self, now: Instant) -> Option<T> {
        if self.next_send.is_some_and(|at| now < at) {
            return None;
        }
        let value = self.value.take()?;
        // Throttle failures too; prevent busy-loop.
        self.next_send = Some(now + self.interval);
        Some(value)
    }

    /// Puts back a value whose send failed, unless a newer one is already waiting. It goes out
    /// after the interval `take_due` started.
    pub fn retry(&mut self, value: T) {
        if self.value.is_none() {
            self.offer(value);
        }
    }

    pub fn sent(&mut self, value: T, now: Instant) {
        self.sent = Some(value);
        self.next_send = Some(now + self.interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failed send is retried after the interval, and loses to a newer state.
    #[test]
    fn a_failed_send_is_retried_once_unless_superseded() {
        let t0 = Instant::now();
        let mut p = Pending::new(Duration::from_millis(10));
        p.offer(1u8);
        assert_eq!(p.take_due(t0), Some(1));
        p.retry(1);
        assert_eq!(p.take_due(t0), None, "the retry waits out the interval");
        assert_eq!(p.take_due(t0 + Duration::from_millis(10)), Some(1));

        p.offer(2);
        assert_eq!(p.take_due(t0 + Duration::from_millis(20)), Some(2));
        p.offer(3);
        p.retry(2);
        assert_eq!(
            p.take_due(t0 + Duration::from_millis(30)),
            Some(3),
            "the newer state wins"
        );
    }
}
