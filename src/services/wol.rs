//! Client-side WOL: parse stored MACs and send magic packets (shared with other clients).
//! Broadcast reaches sleeping hosts; unicast added when `last_ip` known.
use std::net::Ipv4Addr;

/// Send magic packet to parseable MACs. Returns true if at least one sent.
fn wake(macs: &[String], last_ip: Option<Ipv4Addr>) -> bool {
    let parsed: Vec<[u8; 6]> = macs.iter().filter_map(|s| punktfunk_core::wol::parse_mac(s)).collect();
    if parsed.is_empty() {
        return false;
    }
    punktfunk_core::wol::send_magic_packet(&parsed, last_ip).is_ok()
}

/// Send WOL packet + log outcome. `name` is for readable log only.
pub fn wake_and_log(macs: &[String], last_ip: Option<Ipv4Addr>, name: &str) -> bool {
    let ok = wake(macs, last_ip);
    tracing::info!("wake-on-lan: sent to {name} ({} mac(s)), ok={ok}", macs.len());
    ok
}
