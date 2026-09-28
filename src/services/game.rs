//! Ending a launched game over the management REST API: `POST
//! https://<host>:<mgmt_port>/api/v1/game/end`, on the mTLS lane and pinned identity
//! [`super::power`] uses. The host ends a title only when this device launched it.

use crate::services::library::{agent_within, base_url, classify};

/// What asking the host to end a game came to. The desktop and mobile clients' `GameEnd`,
/// words included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GameEnd {
    Ended,
    /// 409: the host had nothing of this title left to end.
    NotRunning,
    /// 401/404: a host that predates ending games from a device.
    Unsupported,
    /// 403: this device's access to the host expired.
    Expired,
    Failed(String),
}

impl GameEnd {
    pub fn from_status(code: u16) -> Self {
        match code {
            200..=299 => Self::Ended,
            409 => Self::NotRunning,
            401 | 404 => Self::Unsupported,
            403 => Self::Expired,
            code => Self::Failed(format!("the host refused it ({code})")),
        }
    }

    /// The line the Home status bar shows.
    pub fn notice(&self, title: &str) -> String {
        match self {
            Self::Ended => format!("Ended {title}."),
            Self::NotRunning => format!("{title} isn't running any more."),
            Self::Unsupported => "This host needs an update to end games from here.".into(),
            Self::Expired => "This device's access to the host has expired.".into(),
            Self::Failed(why) => format!("Couldn't end {title} \u{2014} {why}"),
        }
    }
}

/// Ends `app_id` on the host, live session included. Blocking — call it from a worker.
pub fn end(
    addr: &str,
    mgmt_port: u16,
    identity: &(String, String),
    pin: Option<[u8; 32]>,
    app_id: &str,
    budget: std::time::Duration,
) -> GameEnd {
    let agent = match agent_within(identity, pin, budget) {
        Ok(agent) => agent,
        Err(e) => return GameEnd::Failed(e.to_string()),
    };
    let url = format!("{}/api/v1/game/end", base_url(addr, mgmt_port));
    let body = serde_json::json!({ "app_id": app_id, "streaming": true }).to_string();
    match agent
        .post(url.as_str())
        .header("Content-Type", "application/json")
        .send(body.as_bytes())
    {
        Ok(_) => GameEnd::Ended,
        Err(ureq::Error::StatusCode(code)) => GameEnd::from_status(code),
        Err(e) => GameEnd::Failed(classify(e).to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each status says one thing to the player, in the words every other client uses.
    #[test]
    fn a_status_maps_to_what_the_player_is_told() {
        assert_eq!(GameEnd::from_status(200), GameEnd::Ended);
        assert_eq!(GameEnd::from_status(409), GameEnd::NotRunning);
        assert_eq!(GameEnd::from_status(401), GameEnd::Unsupported);
        assert_eq!(GameEnd::from_status(404), GameEnd::Unsupported);
        assert_eq!(GameEnd::from_status(403), GameEnd::Expired);
        assert_eq!(GameEnd::NotRunning.notice("Hades"), "Hades isn't running any more.");
    }
}
