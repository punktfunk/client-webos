//! `/proc/bus/input/devices`: one record per input node, blank-line separated — the one place the
//! kernel publishes a node's bus, its handlers and its `Uniq` (a pad's Bluetooth MAC) side by side.
//! Readable in the app jail; the pad-link keeper and the `DualSense` feedback routing both read it.

/// Every node's record.
pub(crate) fn records(devices: &str) -> impl Iterator<Item = &str> {
    devices.split("\n\n")
}

/// Whether the record's node is on Bluetooth (`I: Bus=0005`; USB is `0003`).
pub(crate) fn is_bluetooth(record: &str) -> bool {
    record.lines().any(|l| l.trim().starts_with("I: Bus=0005"))
}

/// The record's handler names (`H: Handlers=kbd event7 js0` → `kbd`, `event7`, `js0`).
pub(crate) fn handlers(record: &str) -> impl Iterator<Item = &str> {
    record
        .lines()
        .filter_map(|l| l.trim().strip_prefix("H: Handlers="))
        .flat_map(str::split_whitespace)
}

/// The record's `U: Uniq=`, lowercased; `None` when absent or empty.
pub(crate) fn uniq(record: &str) -> Option<String> {
    record
        .lines()
        .find_map(|l| l.trim().strip_prefix("U: Uniq="))
        .map(|u| u.trim().to_ascii_lowercase())
        .filter(|u| !u.is_empty())
}
