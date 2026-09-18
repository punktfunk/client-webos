//! The `webosbrew/SDL-webOS` fork's own entry points, resolved at runtime rather than linked.
//!
//! Same rule as `ndl::ffi` (`docs/NOTES.md`): only the bundled fork exports these, so linking
//! them would stop the process before `main()` under a stock SDL3. Resolved here, a miss is a
//! runtime error every caller degrades on.
use std::ffi::CStr;
use std::sync::OnceLock;

use anyhow::Result;

use super::dl;

const LIB_NAME: &CStr = c"libSDL3.so.0";

/// The fork-only functions this app calls. One table: they ship together, so they fail together.
pub struct Fns {
    /// `false` self-gates on TVs without `wl_webos_input_manager`.
    pub cursor_visibility: unsafe extern "C" fn(bool) -> bool,
    /// Panel size, not surface. Stock SDL maps app plane to 1080p on 4K.
    pub panel_resolution: unsafe extern "C" fn(*mut i32, *mut i32) -> bool,
    /// Panel refresh rate in Hz.
    pub refresh_rate: unsafe extern "C" fn(*mut i32) -> bool,
}

/// Resolved once; a miss is a named error the callers degrade on.
pub fn fns() -> Result<&'static Fns> {
    static FNS: OnceLock<std::result::Result<Fns, String>> = OnceLock::new();
    dl::cached(&FNS, LIB_NAME, |lib| {
        Ok(Fns {
            cursor_visibility: lib.sym(c"SDL_webOSCursorVisibility")?,
            panel_resolution: lib.sym(c"SDL_webOSGetPanelResolution")?,
            refresh_rate: lib.sym(c"SDL_webOSGetRefreshRate")?,
        })
    })
}
