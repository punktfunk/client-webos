//! The shared gamepad shell (`pf-console-ui`) hosted on this client's SDL window.
//!
//! Two halves, mirroring the Android client's `clients/android/native/src/console/`: [`gl`]
//! owns the GL context and the Skia surface, [`model`] owns everything the shell asks the
//! binary to do. The frame loop that joins them is `runtime::console_flow`.
//!
//! It reads and writes the ONE settings document (`services::store`) through the app's own
//! writer — see `services::store::console`.

mod gl;
mod model;

pub(crate) use gl::{ConsoleGl, GPU_CACHE_BYTES};
pub(crate) use model::Service;
