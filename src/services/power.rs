//! Host power actions over the management REST API: `POST
//! https://<host>:<mgmt_port>/api/v1/actions/<id>`, on the same mTLS lane and the same pinned
//! identity `library` fetches the game list with. Host-side from `punktfunk-core` 0.33.0.
//!
//! Empty body, no parameters — the id selects a fixed host-side behaviour. On `202` the host
//! ends every live session with a typed close, waits about a second so the response flushes,
//! then acts, so there is nothing for this client to wait around for.
use serde::Deserialize;

use crate::services::library::{agent_within, base_url, classify, get_json, LibraryError};
use crate::services::store::ExitAction;

/// One entry of `GET /api/v1/actions`, trimmed to the two flags this client acts on. Every
/// other field (title, group, danger) is the host describing a row we don't render.
#[derive(Debug, Deserialize)]
struct ActionInfo {
    id: String,
    /// Whether this host can run it right now — a VM that cannot suspend lists sleep as
    /// unavailable rather than offering a dead switch.
    available: bool,
    /// Whether THIS pairing may invoke it: the `GRANT_POWER` bit of its live access mask.
    permitted: bool,
}

#[derive(Debug, Deserialize)]
struct ActionList {
    actions: Vec<ActionInfo>,
}

/// Which power actions this pairing may actually invoke on a host, as the host itself reports
/// them.
///
/// Per action rather than one yes/no, because the two flags behind it are per action: a VM
/// that cannot suspend lists sleep unavailable while shutdown stays fine. Availability and
/// permission are folded together — an action refused for either reason is equally unusable,
/// and nothing this client does with the answer needs to tell them apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PowerRights {
    pub sleep: bool,
    pub shutdown: bool,
}

impl PowerRights {
    /// Whether one specific pick would be accepted. [`ExitAction::None`] sends nothing, so it
    /// is always allowed.
    pub fn allows(self, action: ExitAction) -> bool {
        match action {
            ExitAction::None => true,
            ExitAction::Sleep => self.sleep,
            ExitAction::Shutdown => self.shutdown,
        }
    }
}

/// Asks the host which power actions this pairing may invoke, blocking.
///
/// `Ok(PowerRights::default())` — the host answered and offers nothing — is a real answer, not
/// an error: no Host power grant on this pairing, or a platform with no executor. Only a
/// transport failure is `Err`.
pub(crate) fn probe_rights(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<PowerRights, LibraryError> {
    let usable = |actions: &[ActionInfo], id: &str| actions.iter().any(|a| a.id == id && a.available && a.permitted);
    match fetch_actions(addr, mgmt_port, identity, pin) {
        Ok(actions) => Ok(PowerRights {
            sleep: usable(&actions, "power.sleep"),
            shutdown: usable(&actions, "power.shutdown"),
        }),
        // 401/403 is the host answering and offering us nothing — an access mask with no Host
        // power grant, which is a real answer. Everything else is passed up for the caller to
        // classify: a 404 (host older than the actions route) is not the same as a refusal,
        // and telling someone to widen a grant their host has never heard of is worse than
        // saying nothing.
        Err(LibraryError::NotPaired) => Ok(PowerRights::default()),
        Err(e) => {
            tracing::debug!("power rights probe ({addr}:{mgmt_port}): {e}");
            Err(e)
        }
    }
}

fn fetch_actions(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
) -> Result<Vec<ActionInfo>, LibraryError> {
    get_json::<ActionList>(
        addr,
        mgmt_port,
        identity,
        pin,
        "/api/v1/actions",
        crate::services::budget::REQUEST,
    )
    .map(|l| l.actions)
}

/// Invokes one action by id, blocking. `Ok(())` means the host accepted it (202) — not that it
/// ran: the executor is on the other side of a deliberate grace period, and by design the
/// process that would report failure is the one going down.
///
/// The interesting refusals are classified rather than swallowed, because the caller's only
/// user-visible output is a log line: 403 is "this pairing has no Host power grant", 409 is
/// "another device is still streaming", 501 is "this host platform has no executor" (macOS).
pub fn invoke(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    action_id: &str,
    budget: std::time::Duration,
) -> Result<(), LibraryError> {
    let agent = agent_within(identity, pin, budget)?;
    let url = format!("{}/api/v1/actions/{action_id}", base_url(addr, mgmt_port));
    match agent.post(url.as_str()).send_empty() {
        Ok(_) => Ok(()),
        Err(e) => Err(classify(e)),
    }
}

/// Everything one exit action needs, captured while the console's `Service` is still alive.
///
/// Carried rather than looked up at exit time because the two paths that quit the process are
/// on opposite sides of the menu: one still holds the `Service`, and the other (a Quit out of the
/// stream) never returns to the menu at all.
#[derive(Clone)]
pub struct ExitPlan {
    pub addr: String,
    pub mgmt_port: u16,
    pub identity: (String, String),
    pub pin: Option<[u8; 32]>,
    /// What to do. Never [`ExitAction::None`] — a plan only exists for a host set to actually
    /// do something, which is what [`ExitAction::action_id`] returning `None` rules out.
    pub action: ExitAction,
}

/// Hand-written so a stray `{:?}` can't put `identity`'s client key PEM in the log.
impl std::fmt::Debug for ExitPlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExitPlan")
            .field("addr", &self.addr)
            .field("mgmt_port", &self.mgmt_port)
            .field("action", &self.action)
            .finish_non_exhaustive()
    }
}

impl ExitPlan {
    /// Sends the action within `budget`. `Ok(())` means accepted (202), not that it ran.
    pub fn send_within(&self, budget: std::time::Duration) -> Result<(), LibraryError> {
        let Some(action_id) = self.action.action_id() else {
            return Ok(());
        };
        tracing::info!("power action: {action_id} on {}", self.addr);
        invoke(&self.addr, self.mgmt_port, &self.identity, self.pin, action_id, budget)
    }

    /// Fires the action on the way out, blocking for at most one management-API request.
    ///
    /// Best-effort by design: the app is quitting either way, and there is no screen left to
    /// report a failure on — so the outcome goes to the log and nothing else.
    pub fn run(&self) {
        match self.send_within(crate::services::budget::EXIT_ACTION) {
            Ok(()) => tracing::info!("exit action accepted"),
            Err(e) => tracing::warn!("exit action refused: {e}"),
        }
    }
}
