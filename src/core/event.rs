/// How long a button must be held to take a gesture's long-press branch: the card's own
/// menu instead of launch on a Home card (`runtime::input::CARD_HOLD`), right-click instead
/// of left on the Magic Remote's OK during a stream
/// (`platform::webos::mouse::OkPress`). One value so a hold feels the same
/// wherever the user learns it.
pub const LONG_PRESS: std::time::Duration = std::time::Duration::from_millis(500);

/// Menu event (debounced from raw SDL input: keyboard arrows, gamepad d-pad).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MenuEvent {
    Up,
    Down,
    Left,
    Right,
    Confirm,
    Back,
    /// "Forget host" (separate from Back/Confirm to prevent accident).
    Secondary,
}

impl MenuEvent {
    /// Whether this is one of the four navigation directions — the events that move focus and
    /// so are the ones a held control repeats.
    pub fn is_directional(self) -> bool {
        matches!(self, Self::Up | Self::Down | Self::Left | Self::Right)
    }
}
