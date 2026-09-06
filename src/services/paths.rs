//! Where this app keeps its files.
//!
//! Its own module, rather than a private helper in `store`, so a probe binary can pull in one
//! leaf module instead of the whole persistence layer.
use std::path::PathBuf;

/// The app's data directory: `$HOME` under webOS's per-app jail, `/tmp` if it is unset (a
/// bare SSH shell), which keeps a dev run working without writing somewhere unexpected.
pub fn app_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("/tmp"), PathBuf::from)
}

/// The one charset every packaged asset name and every mDNS-advertised OS token is held to.
pub fn is_asset_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}
