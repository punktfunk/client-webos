//! "Wake this host?" confirmation. Logic lives in `app::state::wake`.
//!
//! The dialog itself is `app::view::confirm` — this is only its copy. A host with no MAC on
//! record never opens it (`state::wake::wake_prompts`): [`status_text`] goes on the Home
//! status line instead, since there would be nothing on the card to press.
use crate::app::WakeState;

pub const TITLE: &str = "Wake this host?";

/// Status line; reconstructible from `wake` alone, so render and layout can't disagree.
pub(crate) fn status_text(wake: &WakeState) -> String {
    if wake.mac.is_empty() {
        format!(
            "{} isn't responding, and no Wake-on-LAN address is on record for it yet, so it \
             can't be woken from here. It will reconnect automatically once it's back online.",
            wake.name
        )
    } else {
        format!("{} isn't responding. It may be powered off or asleep.", wake.name)
    }
}
