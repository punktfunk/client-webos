//! This app's icon vocabulary: which Lucide mark means "settings", "wake", "forget". The
//! names are the kit's (`pf_console_ui::icons::by_name`), the same table every client draws.

pub const ICON_TV: &str = "tv";
pub const ICON_LOCK: &str = "lock";
pub const ICON_ADD: &str = "plus";
pub const ICON_SETTINGS: &str = "settings";
pub const ICON_SIGNAL: &str = "activity";
pub const ICON_POWER: &str = "power";
pub const ICON_DELETE: &str = "trash-2";
pub const ICON_EDIT: &str = "pencil";
pub const ICON_MORE: &str = "ellipsis";
pub const ICON_SEND: &str = "send";
pub const ICON_REORDER: &str = "grip-vertical";
pub const ICON_CLOSE: &str = "x";
pub const ICON_WRENCH: &str = "wrench";
pub const ICON_PLAY: &str = "play";
pub const ICON_PIN: &str = "pin";
/// The Desktop card's own mark, for a host whose OS it cannot wear. Mirrors the kit's
/// `library::DESKTOP_ICON`, which the pinned revision predates — read it from there once
/// the pin moves.
pub const ICON_DESKTOP: &str = "monitor";
