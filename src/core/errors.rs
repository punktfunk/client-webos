//! User-facing sentences for `punktfunk-core`'s error and rejection types, and for a refusal
//! this client raises itself.
//!
//! A host's refusal is worded by `pf_client_core::trust`; the rest come from `pf-l10n`. Without
//! this, failures render as Debug strings (e.g. "connect: Rejected(Busy)").
use pf_client_core::trust;
use punktfunk_core::PunktfunkError;

use crate::core::media::DecoderBlocked;

/// Why connect/probe failed (distinguishes rejection from transport trouble). A host's own
/// sentence knows more than its reason code, so `trust` lets it win.
fn connect_message(err: &PunktfunkError) -> String {
    match err {
        PunktfunkError::Rejected(reason, said) => trust::reject_message(*reason, Some(said)),
        PunktfunkError::Timeout => pf_l10n::client::error_host_timeout().into(),
        PunktfunkError::Io(_) => pf_l10n::client::error_host_unreachable().into(),
        PunktfunkError::Closed => pf_l10n::console::console_error_host_closed().into(),
        other => pf_l10n::console::console_error_connect_failed_detail(other),
    }
}

/// Why PIN pairing failed (Crypto = wrong PIN, not network problem).
pub fn pair_message(err: &PunktfunkError) -> String {
    match err {
        PunktfunkError::Crypto => pf_l10n::client::pair_wrong_pin().into(),
        other => connect_message(other),
    }
}

/// Why a request for access failed. It has no PIN, so Crypto is a host that didn't match its advert.
pub fn access_message(err: &PunktfunkError) -> String {
    match err {
        PunktfunkError::Crypto => pf_l10n::client::pair_identity_mismatch().into(),
        other => connect_message(other),
    }
}

/// The sentence for a failed connect. A blocked decoder and a `PunktfunkError` have their own
/// words under any context; anything else reads as its log text.
pub fn friendly(err: &anyhow::Error) -> String {
    if err.is::<DecoderBlocked>() {
        return pf_l10n::console::console_webos_decoder_blocked().into();
    }
    err.downcast_ref::<PunktfunkError>()
        .map_or_else(|| format!("{err:#}"), connect_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use punktfunk_core::reject::RejectReason;

    #[test]
    fn a_hosts_own_sentence_wins_over_the_reason_line() {
        let said = PunktfunkError::Rejected(RejectReason::NoSeat, "Seat 2 is in use.".into());
        assert_eq!(connect_message(&said), "Seat 2 is in use.");
        let silent = PunktfunkError::Rejected(RejectReason::NoSeat, " ".into());
        assert_eq!(connect_message(&silent), "All seats are taken.");
    }

    #[test]
    fn a_blocked_decoder_names_its_fix_and_the_log_keeps_the_device() {
        let blocked = DecoderBlocked { machine: "k5lp".into() };
        let err = anyhow::Error::new(blocked).context("NDL v1 load");
        assert_eq!(friendly(&err), pf_l10n::console::console_webos_decoder_blocked());
        assert!(format!("{err:#}").contains("/dev/rtkmem unreadable (machine k5lp)"));
    }
}
