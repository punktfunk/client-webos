//! LAN discovery via mDNS. Direct mdns-sd dep to avoid pf-client-core's FFmpeg/PipeWire.
use mdns_sd::{ServiceDaemon, ServiceEvent};

/// mDNS service type punktfunk hosts advertise.
pub const SERVICE_TYPE: &str = "_punktfunk._udp.local.";

#[derive(Clone, Debug)]
pub struct DiscoveredHost {
    pub name: String,
    pub addr: String,
    pub port: u16,
    /// Management API port from mDNS (None → `library::DEFAULT_MGMT_PORT`).
    pub mgmt_port: Option<u16>,
    /// Wake-on-LAN MACs from mDNS (learned while awake, persisted to `KnownHost`).
    pub mac: Vec<String>,
    /// Generic-to-specific OS identity chain, such as `linux/fedora/bazzite`.
    pub os: String,
}

/// IPv4 address and short instance name from a resolved record. IPv4 only (same as other
/// clients).
///
/// The resolved set is a union of every responder's answer, so a host on a VPN or overlay
/// network contributes addresses we may not be able to route. Ranking is
/// `punktfunk_core::discovery`'s, shared with the desktop and Android clients: picking
/// arbitrarily out of the set re-rolled the dial address on every re-announce.
fn addr_and_name(info: &mdns_sd::ResolvedService) -> Option<(String, String)> {
    let candidates: Vec<std::net::Ipv4Addr> = info.get_addresses_v4().iter().copied().collect();
    // Advisory TXT: the host names which address its advert is for. Absent on older hosts,
    // where the ranking's remaining rungs still settle the tie.
    let declared = info
        .get_properties()
        .get_property_val_str("addr")
        .and_then(|v| v.parse().ok());
    let Some(addr) = punktfunk_core::discovery::pick_host_addr(&candidates, declared) else {
        tracing::warn!("mdns: resolved {} with no IPv4 address, skipping", info.get_fullname());
        return None;
    };
    Some((
        addr.to_string(),
        info.get_fullname().split('.').next().unwrap_or("?").to_string(),
    ))
}

/// Trims an advertised OS chain to something bounded and predictable: lowercase, at most five
/// tokens of 32 characters, nothing outside [`paths::is_asset_char`]'s charset. Nothing
/// downstream trusts a host's strings, and this one is matched against packaged mark names.
fn sanitize_os(raw: &str) -> String {
    raw.to_lowercase()
        .split('/')
        .filter_map(|token| {
            let token: String = token
                .chars()
                .filter(|c| crate::services::paths::is_asset_char(*c))
                .take(32)
                .collect();
            // A token of dots alone names nothing (and was once a path segment).
            (!token.is_empty() && token.bytes().any(|b| b != b'.')).then_some(token)
        })
        .take(5)
        .collect::<Vec<_>>()
        .join("/")
}

/// The advert's TXT fields as the record keeps them: management port, wake MACs, OS chain.
/// Nothing here is trusted — a bad port reads as absent and the chain is bounded.
fn txt_fields(mgmt: Option<&str>, mac: Option<&str>, os: Option<&str>) -> (Option<u16>, Vec<String>, String) {
    (
        mgmt.and_then(|v| v.parse().ok()),
        mac.unwrap_or("")
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        sanitize_os(os.unwrap_or("")),
    )
}

/// Turns a resolved record into a host, or `None` if it isn't usable (no IPv4).
fn parse_discovery(info: &mdns_sd::ResolvedService) -> Option<DiscoveredHost> {
    let (addr, name) = addr_and_name(info)?;
    let props = info.get_properties();
    let (mgmt_port, mac, os) = txt_fields(
        props.get_property_val_str("mgmt"),
        props.get_property_val_str("mac"),
        props.get_property_val_str("os"),
    );
    Some(DiscoveredHost {
        name,
        addr,
        port: info.get_port(),
        mgmt_port,
        mac,
        os,
    })
}

/// mdns-sd doubles its PTR re-query interval every round (1s, 2s ... capped at an hour) and
/// discards a whole incoming message on any parse error, so answers lost inside one malformed
/// packet can leave the list empty for minutes. Restarting resets that backoff, but also its
/// traffic-quieting — hence this long, not shorter.
const REBROWSE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(120);

/// A running browse for punktfunk hosts, drained from the menu tick. No thread of its own —
/// mdns-sd's daemon already runs one. That daemon re-queries on its own timers, so what keeps
/// discovery off the network during a stream is `App` dropping this (and with it the daemon)
/// when the menu loop exits, not the tick.
pub struct Discovery {
    daemon: ServiceDaemon,
    events: mdns_sd::Receiver<ServiceEvent>,
    last_browse: std::time::Instant,
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

impl Discovery {
    /// `None` if the daemon or initial browse won't start — discovery is then simply absent.
    pub fn start() -> Option<Self> {
        let daemon = ServiceDaemon::new()
            .inspect_err(|e| tracing::error!("mdns: ServiceDaemon::new failed: {e}"))
            .ok()?;
        let events = match daemon.browse(SERVICE_TYPE) {
            Ok(events) => events,
            Err(e) => {
                // The daemon's thread outlives its handle, so a failed start still has to stop it.
                tracing::error!("mdns: browse({SERVICE_TYPE}) failed: {e}");
                let _ = daemon.shutdown();
                return None;
            }
        };
        tracing::debug!("mdns: browsing {SERVICE_TYPE}");
        Some(Self {
            daemon,
            events,
            last_browse: std::time::Instant::now(),
        })
    }

    /// Hosts resolved since the last call, restarting the browse when due. Empty on almost
    /// every tick, which costs no allocation.
    pub fn poll(&mut self) -> Vec<DiscoveredHost> {
        let mut found = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            let ServiceEvent::ServiceResolved(info) = event else {
                tracing::debug!("mdns: {event:?}");
                continue;
            };
            let Some(host) = parse_discovery(&info) else {
                continue;
            };
            tracing::info!("mdns: resolved {} at {}:{}", host.name, host.addr, host.port);
            found.push(host);
        }
        // After the drain, not before: re-browsing swaps the receiver out and whatever it still
        // held goes with it.
        if self.last_browse.elapsed() >= REBROWSE_INTERVAL {
            self.rebrowse();
        }
        found
    }

    fn rebrowse(&mut self) {
        // Without the stop, the old retransmission chain keeps running and they stack one per
        // interval. It also drops the cache, which is what makes hosts resolve from scratch.
        let _ = self.daemon.stop_browse(SERVICE_TYPE);
        match self.daemon.browse(SERVICE_TYPE) {
            Ok(events) => self.events = events,
            // The stop already removed the querier, so nothing more arrives until the
            // next interval retries this.
            Err(e) => tracing::error!("mdns: re-browse({SERVICE_TYPE}) failed: {e}"),
        }
        self.last_browse = std::time::Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host's TXT strings are input: the OS chain is lowercased, bounded and stripped to
    /// the mark charset, MACs split on commas with blanks dropped, a bad port reads as absent.
    #[test]
    fn txt_fields_are_bounded_and_never_trusted() {
        let (mgmt, mac, os) = txt_fields(
            Some("47990"),
            Some("aa:bb:cc:dd:ee:ff, , 11:22:33:44:55:66"),
            Some("Linux/Fedora/Bazzite"),
        );
        assert_eq!(mgmt, Some(47990));
        assert_eq!(mac, vec!["aa:bb:cc:dd:ee:ff", "11:22:33:44:55:66"]);
        assert_eq!(os, "linux/fedora/bazzite");
        let (mgmt, mac, os) = txt_fields(Some("port"), None, Some("a/b/c/d/e/f/g"));
        assert_eq!(mgmt, None);
        assert!(mac.is_empty());
        assert_eq!(os, "a/b/c/d/e", "five tokens at most");
        assert_eq!(sanitize_os("../Win dows!//"), "windows");
        assert_eq!(sanitize_os(&"x".repeat(40)), "x".repeat(32));
    }
}
