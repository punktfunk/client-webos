//! The settings document and the UI editing it.
//!
//! `settings` is the live global document — the value the stream is launched with, and what the
//! Settings screen writes to. The rest is the editing surface around it: which per-game override
//! is open, the dropdown overlay and its fade, and whether the bitrate slider is being dragged.

use pf_client_core::trust::Settings;

pub(crate) struct SettingsUi {
    pub(crate) settings: Settings,
    /// Whether the mouse button is down on the Settings screen's slider row (Bitrate) with the
    /// press having landed on the track itself — while `true`, `MouseMotion` drags the thumb to
    /// the pointer's x instead of just moving hover focus. Cleared on `MouseButtonUp`; never
    /// survives a screen change since the button can't be released on another screen from
    /// webOS's own D-pad OK -> click translation.
    pub(crate) slider_drag: bool,
}

impl SettingsUi {
    pub(crate) fn new(settings: Settings) -> Self {
        Self {
            settings,
            slider_drag: false,
        }
    }
}
