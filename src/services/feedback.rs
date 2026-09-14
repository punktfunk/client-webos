//! Latest-state delivery, with transport deadlines independent of incoming events.
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

    /// A final release cannot be overwritten by later ordinary updates.
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
        // Failed attempts are throttled too; a broken service must not become a busy loop.
        self.next_send = Some(now + self.interval);
        Some(value)
    }

    pub fn sent(&mut self, value: T, now: Instant) {
        self.sent = Some(value);
        self.next_send = Some(now + self.interval);
    }
}
