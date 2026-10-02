//! The ST.2084 (PQ) transfer function.
//!
//! PQ is absolute: a code means a luminance, independently of the display. That is the whole
//! premise of the HDR calibration — ask the panel for 1200 nits and see whether it delivers.

/// 10-bit narrow-range luma endpoints (ITU-R BT.2020 / ST.2084 signal range).
pub const BLACK_CODE: u16 = 64;
pub const WHITE_CODE: u16 = 940;

// ST.2084 (PQ) constants.
const M1: f32 = 2610.0 / 16384.0;
const M2: f32 = 2523.0 / 4096.0 * 128.0;
const C1: f32 = 3424.0 / 4096.0;
const C2: f32 = 2413.0 / 4096.0 * 32.0;
const C3: f32 = 2392.0 / 4096.0 * 32.0;

/// Nits to a 10-bit narrow-range luma code through the inverse EOTF.
#[must_use]
pub fn pq_code(nits: f32) -> u16 {
    let y = (nits.max(0.0) / 10_000.0).powf(M1);
    let e = ((C1 + C2 * y) / (1.0 + C3 * y)).powf(M2);
    let code = f32::from(BLACK_CODE) + e * f32::from(WHITE_CODE - BLACK_CODE);
    // Rounds to the nearest code rather than truncating: a half-code bias at the top of the range
    // is a visible step on the patches this is measuring with.
    (code.round() as i32).clamp(i32::from(BLACK_CODE), i32::from(WHITE_CODE)) as u16
}

/// The luminance a code stands for — [`pq_code`] the other way round. The stored black level is a code rather than nits, because
/// near the floor several decades of nits share one code.
#[must_use]
pub fn pq_nits(code: u16) -> f32 {
    let e = f32::from(code.clamp(BLACK_CODE, WHITE_CODE) - BLACK_CODE) / f32::from(WHITE_CODE - BLACK_CODE);
    let ep = e.powf(1.0 / M2);
    ((ep - C1).max(0.0) / (C2 - C3 * ep)).powf(1.0 / M1) * 10_000.0
}
