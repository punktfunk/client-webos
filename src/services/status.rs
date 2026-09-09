//! What a host has launched right now: `GET https://<host>:<mgmt_port>/api/v1/status`.
//!
//! Its own module rather than another endpoint in [`super::library`], which is scoped to the
//! one-shot library fetch and surfaces its failures to the user. This resource is polled, and
//! its failures are silent by design — see [`fetch_running`]. Both share that module's mTLS
//! primitives (`agent`, `get_json`, `base_url`), which is the seam they exist for.

use super::library::get_json;

/// One title the host currently has launched, from `GET /api/v1/status`. A partial decode of
/// the host's `ActiveGame`: session, plane and grace stay undecoded, so the dot does not break
/// when the operator payload grows.
#[derive(serde::Deserialize)]
struct RunningGame {
    /// Store-qualified id (`steam:570`) — the join key onto the library's own
    /// [`crate::core::model::GameEntry::id`]. Absent for an operator-typed `GameStream`
    /// command, which has no catalog row to mark.
    #[serde(default)]
    app_id: Option<String>,
    /// `launching` | `running` | `exited` | `untracked` | `grace`. A `String` so a host value
    /// this build does not know cannot fail the whole list decode.
    #[serde(default)]
    state: String,
}

/// The `/api/v1/status` slice this client reads. Everything else the operator payload carries
/// stays undecoded, per [`RunningGame`].
#[derive(serde::Deserialize)]
struct HostStatus {
    #[serde(default)]
    games: Vec<RunningGame>,
}

/// The ids of what `addr` has launched right now.
///
/// Best-effort by contract: a host too old to serve `/api/v1/status`, one that is unreachable,
/// or a payload shaped differently all read as "nothing running". A dot that fails to light is
/// cheaper than an error the Home screen would have to explain.
pub(crate) fn fetch_running(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    budget: std::time::Duration,
) -> Vec<String> {
    match get_json::<HostStatus>(addr, mgmt_port, identity, pin, "/api/v1/status", budget) {
        Ok(status) => running_ids(status),
        Err(e) => {
            tracing::debug!("running: {addr}:{mgmt_port} — {e}");
            Vec::new()
        }
    }
}

/// The ids worth marking, out of what the host reported: what is up, and what names a catalog
/// row. An operator-typed `GameStream` command carries no `app_id` and so has no card to light.
fn running_ids(status: HostStatus) -> Vec<String> {
    status
        .games
        .into_iter()
        // Anything but `exited` is up: `untracked` (the host cannot follow the process) and
        // `grace` (the session is gone, the process is not) both still light the dot.
        .filter(|g| g.state != "exited")
        .filter_map(|g| g.app_id)
        .collect()
}

/// One `fetch_running` answer, tagged with the host it describes: the poll outlives a host
/// switch, and a late answer must not light a card in the library that replaced it.
pub(crate) struct RunningLoaded {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) running: Vec<String>,
}

/// Spawns [`fetch_running`] on a worker — the same shape as
/// [`super::library::load_games_async`], and for the same reason: this is a blocking mTLS
/// round-trip, and the menu loop must not wait on it.
pub(crate) fn load_running_async(
    host: String,
    port: u16,
    mgmt_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
    budget: std::time::Duration,
) -> std::sync::mpsc::Receiver<RunningLoaded> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("punktfunk-webos-running".into())
        .spawn(move || {
            let running = fetch_running(&host, mgmt_port, &identity, fingerprint, budget);
            let _ = tx.send(RunningLoaded { host, port, running });
        })
        .expect("spawn running-fetch thread");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the dot is drawn from: every state but `exited` counts as up, and an entry with no
    /// catalog id is dropped rather than joined onto whatever card sorts first.
    #[test]
    fn running_ids_keep_what_is_up_and_named() {
        let status: HostStatus = serde_json::from_str(
            r#"{"games":[
                {"app_id":"steam:570","state":"running","session":"ignored"},
                {"app_id":"steam:271590","state":"launching"},
                {"app_id":"flatpak:org.x","state":"grace"},
                {"app_id":"heroic:1","state":"untracked"},
                {"app_id":"steam:4000","state":"exited"},
                {"state":"running"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            running_ids(status),
            ["steam:570", "steam:271590", "flatpak:org.x", "heroic:1"]
        );
    }

    /// A host too old to serve the field, and one whose payload grew a shape this build does
    /// not know: both read as "nothing running", never as a decode failure.
    #[test]
    fn an_unknown_status_payload_lights_nothing() {
        let empty: HostStatus = serde_json::from_str("{}").unwrap();
        assert!(running_ids(empty).is_empty());
        let odd: HostStatus = serde_json::from_str(r#"{"games":[],"displays":[{"id":1}]}"#).unwrap();
        assert!(running_ids(odd).is_empty());
    }
}
