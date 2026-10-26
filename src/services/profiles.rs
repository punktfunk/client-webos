//! A host's profiles over the management API, on the pinned mTLS lane `library` uses:
//! `GET /api/v1/profiles/enumerate` for the console's picker, and `POST
//! /api/v1/profiles/{id}/wake` to start a stopped seat. The rules for using the list live in
//! `pf_client_core::profiles`, shared with every other client.

use pf_client_core::profiles::ListedProfile;

use crate::services::budget;
use crate::services::library::{agent, base_url, classify, get_json, LibraryError};

/// The profiles on the box. `Ok(None)`: it has none, or predates the route (404).
pub fn fetch(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: [u8; 32],
) -> Result<Option<Vec<ListedProfile>>, LibraryError> {
    let path = "/api/v1/profiles/enumerate";
    none_on_404(get_json(addr, mgmt_port, identity, Some(pin), path, budget::REQUEST))
}

fn none_on_404<T>(answer: Result<T, LibraryError>) -> Result<Option<T>, LibraryError> {
    match answer {
        Ok(listed) => Ok(Some(listed)),
        Err(LibraryError::Http(404)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Starts profile `id`'s stopped seat. The host answers before the seat is up; the console
/// polls [`fetch`] until it is.
pub fn wake(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: [u8; 32],
    id: &str,
) -> Result<(), LibraryError> {
    let url = format!("{}/api/v1/profiles/{id}/wake", base_url(addr, mgmt_port));
    agent(identity, Some(pin))?
        .post(url.as_str())
        .send_empty()
        .map(drop)
        .map_err(classify)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a 404 means "no profiles": any other failure is the host not answering the ask.
    #[test]
    fn only_a_404_reads_as_no_profiles() {
        assert!(matches!(none_on_404(Ok(1)), Ok(Some(1))));
        assert!(matches!(none_on_404::<u8>(Err(LibraryError::Http(404))), Ok(None)));
        assert!(matches!(
            none_on_404::<u8>(Err(LibraryError::NotPaired)),
            Err(LibraryError::NotPaired)
        ));
    }
}
