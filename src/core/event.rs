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
