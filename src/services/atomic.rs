//! Crash-safe file writes.
//!
//! Write-then-rename, never truncate-in-place: `std::fs::write` truncates first, so a
//! kill/power-cut mid-write (this is a TV — losing power IS the off switch) leaves a half-file,
//! and the loaders' `unwrap_or_default()` would then silently discard every paired host / all
//! settings. A rename on the same filesystem is atomic; readers see the old file or the new one,
//! never a torn one.
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Durable: the staged file is synced before the rename. A rename is atomic against a kill, but
/// against a power cut only once the data it points at has reached the disk — otherwise the
/// filesystem may commit the rename first and leave an empty file under the real name. For the
/// files that cannot be refetched: the settings document and the client identity.
pub fn write(path: &Path, contents: &str, what: &str) -> Result<()> {
    stage_and_rename(path, &[contents.as_bytes()], what, true)
}

/// Same discipline for byte payloads that arrive in pieces (a header plus a pixel buffer, say):
/// the parts are written in order, so nothing has to be concatenated into one allocation first.
///
/// Not synced: this is for caches, where a write lost to a power cut costs one refetch, and a
/// sync per entry would cost every fetch.
pub fn write_parts(path: &Path, parts: &[&[u8]], what: &str) -> Result<()> {
    stage_and_rename(path, parts, what, false)
}

/// `.tmp` is appended to the whole filename rather than replacing an extension, so two files
/// that differ only by extension can never stage to the same path.
fn stage_and_rename(path: &Path, parts: &[&[u8]], what: &str, durable: bool) -> Result<()> {
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut file = File::create(&tmp).with_context(|| format!("create {what} (tmp)"))?;
    for part in parts {
        file.write_all(part).with_context(|| format!("write {what} (tmp)"))?;
    }
    if durable {
        file.sync_all().with_context(|| format!("sync {what} (tmp)"))?;
    }
    drop(file);
    std::fs::rename(&tmp, path).with_context(|| format!("rename {what} into place"))
}
