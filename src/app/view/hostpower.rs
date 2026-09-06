//! Per-host power management: waking the host on the way in, and what to do to it on the way
//! out. Logic lives in `app::state::hostpower`.
use crate::app::menu::PowerAccess;
use crate::services::store::ExitAction;
use crate::ui;
use crate::ui::widgets::FocusRow;

/// Names both directions the card covers, since neither row's label says the other exists.
/// Auto-wake's "Off" in particular is not "never wake" but "ask first", which the switch
/// alone can't say.
pub const SUBTITLE: &str = "Wake this host when it's unreachable — off, it asks first — and choose what \
     happens to it when you leave.";
pub const ROW_COUNT: usize = crate::app::menu::POWER_ROW_COUNT;

pub fn title(host_name: &str) -> String {
    format!("Host power · {host_name}")
}

pub fn rows(auto_send: bool, exit_action: ExitAction, access: PowerAccess) -> Vec<FocusRow> {
    // The caption is the picked value's, not the row's: the labels name a power state and
    // "Shut down" alone doesn't say that Wake-on-LAN is then the only way back. A lock's
    // caption replaces it — a row the user can't change has nothing more useful to say than
    // why.
    let exit = FocusRow::dropdown(
        crate::app::view::icons::ICON_POWER,
        "App exit behaviour",
        crate::app::menu::exit_action_label(exit_action),
    );
    let exit = if access.unlocked() {
        exit.with_subtext(caption(exit_action, access))
    } else {
        exit.locked(true).with_subtext(lock_caption(access))
    };
    vec![
        FocusRow::toggle(crate::app::view::icons::ICON_POWER, "Wake automatically", auto_send),
        exit,
    ]
}

/// Why the exit-behaviour row is fixed. Each names what would have to change and where — the
/// grant lives on the host, not on this TV.
fn lock_caption(access: PowerAccess) -> ui::widgets::RowSubtext {
    match access {
        PowerAccess::NotPaired => ui::widgets::RowSubtext::hint("Pair with this host to control its power"),
        PowerAccess::Unknown => ui::widgets::RowSubtext::hint("Checking whether this host allows power control..."),
        PowerAccess::Unreachable => {
            ui::widgets::RowSubtext::hint("Host is not answering — start it to check power access")
        }
        PowerAccess::Unsupported => {
            ui::widgets::RowSubtext::hint("This host is too old for power control — update punktfunk on it")
        }
        // Reached only with no rights at all; `rows` renders the row unlocked otherwise.
        PowerAccess::Rights(_) => {
            ui::widgets::RowSubtext::caution("This device may not control the host's power — grant it on the host")
        }
    }
}

/// What the picked exit behaviour does, on the row itself.
fn caption(exit_action: ExitAction, access: PowerAccess) -> ui::widgets::RowSubtext {
    // A host that offers shutdown but not sleep (a VM that cannot suspend) still has an
    // unlocked row — so the pick that would be refused says so on itself rather than waiting
    // to fail at the moment it matters.
    if !access.allows(exit_action) {
        return ui::widgets::RowSubtext::caution("This host can't do that — pick another");
    }
    let text = crate::app::menu::exit_action_caption(exit_action);
    match exit_action {
        // Losing the host's uptime is the one outcome worth colouring.
        ExitAction::Shutdown => ui::widgets::RowSubtext::caution(text),
        ExitAction::None | ExitAction::Sleep => ui::widgets::RowSubtext::hint(text),
    }
}

/// Why a power action didn't happen. The interesting refusals are named because "nothing
/// happened" otherwise looks identical whether this pairing lacks the Host power grant or the
/// host simply never heard us. Here rather than in `core::errors` because it reads a
/// `services` type, and `core` cannot see `services`.
pub fn refusal_message(e: &crate::services::library::LibraryError) -> String {
    use crate::services::library::LibraryError;
    match e {
        LibraryError::NotPaired => "This device isn't allowed to control the host's power.".into(),
        LibraryError::Http(409) => "The host is refusing — another device is still streaming.".into(),
        LibraryError::Http(501) => "This host can't do that.".into(),
        other => format!("Couldn't reach the host: {other}"),
    }
}
