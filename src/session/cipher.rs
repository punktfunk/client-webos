//! The media cipher this client asks the host for. `pfprobe` includes this file too, so the
//! probe binary and a session never negotiate different ciphers.

use punktfunk_core::quic;

/// `VIDEO_CAP_CHACHA20` only on CPUs without the `ARMv8` AES instructions; with them the host's
/// default AES-GCM is faster. A ≥0.17.2 host picks `ChaCha20` up; older hosts ignore the bit.
pub(crate) fn cipher_caps() -> u8 {
    #[cfg(target_arch = "arm")]
    const HWCAP2_AES: libc::c_ulong = 1;
    #[cfg(target_arch = "arm")]
    // SAFETY: `getauxval` only reads the process's auxiliary vector.
    let has_aes = unsafe { libc::getauxval(libc::AT_HWCAP2) } & HWCAP2_AES != 0;
    #[cfg(not(target_arch = "arm"))]
    let has_aes = true;
    if has_aes {
        0
    } else {
        quic::VIDEO_CAP_CHACHA20
    }
}
