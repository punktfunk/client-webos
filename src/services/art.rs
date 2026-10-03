//! The per-host cover-art disk cache the console shell reads and fills. Covers are kept as
//! small JPEGs, bounded per host, and dropped with the host.
use std::collections::HashSet;
use std::path::PathBuf;

/// Per-host disk budget for covers. Well over a hundred stay resident; a miss costs one LAN
/// fetch.
const CACHE_BUDGET: u64 = 56 * 1024 * 1024;

fn cache_root() -> PathBuf {
    crate::services::paths::app_dir().join("art-cache")
}

fn cache_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// A host's cache directory name. Its own function because [`reconcile_host_caches`] matches
/// directory names against it rather than rebuilding paths.
fn host_key(host: &str, port: u16) -> String {
    cache_name(&format!("{host}_{port}"))
}

fn cache_dir(host: &str, port: u16) -> PathBuf {
    cache_root().join(host_key(host, port))
}

/// Drops every cache directory that isn't one of `known`'s (best-effort).
///
/// The one expression of "a host's art outlives the host exactly as long as the host does".
/// Stated against the whole host list rather than against a single removal, so every way a host
/// can leave is covered by construction — forgetting one, editing its address (a remove plus an
/// upsert), a reset or torn `settings.json`, a migration — rather than each needing its own call
/// at its own site. [`prune_cache`] bounds a host's directory; nothing else bounds their number.
///
/// The filesystem work runs on its own thread: the caller is the menu's bring-up or a keypress
/// (`console::model::Service`), and unlinking a stale host's quota is up to ~190 files.
pub fn reconcile_host_caches(known: &[crate::core::model::KnownHost]) {
    let keep: HashSet<String> = known.iter().map(|h| host_key(&h.addr, h.port)).collect();
    std::thread::Builder::new()
        .name("punktfunk-webos-art-gc".into())
        .spawn(move || {
            let Ok(entries) = std::fs::read_dir(cache_root()) else {
                return;
            };
            for path in entries.flatten().map(|e| e.path()) {
                if path
                    .file_name()
                    .is_some_and(|n| keep.contains(n.to_string_lossy().as_ref()))
                {
                    continue;
                }
                tracing::info!("art: dropping orphaned cache {}", path.display());
                // A stray file rather than a directory is orphaned just the same.
                if std::fs::remove_dir_all(&path).is_err() {
                    let _ = std::fs::remove_file(&path);
                }
            }
        })
        .ok();
}

/// The size a cached cover is kept at. No client draws one larger than a library tile, which is
/// a few hundred pixels wide on a 1080p panel even zoomed; everything above this is decode time
/// nobody sees, paid on every visit.
const COVER_MAX_W: u32 = 480;
const COVER_MAX_H: u32 = 720;
/// Re-encode quality. A cover is photographic and is seen from a couch.
const COVER_QUALITY: u8 = 85;

/// What [`shrink_cover`] makes of a cover's bytes.
enum Shrink {
    /// Already within the cap in both axes AND already JPEG — nothing to gain, and re-encoding
    /// would only lose a generation.
    Keep,
    /// Re-encoded small, as JPEG.
    Shrunk(Vec<u8>),
    /// Not an image this build decodes (`image`'s JPEG and PNG): nothing worth caching.
    Undecodable,
}

/// A cover re-encoded small, as JPEG.
///
/// Only task that actually removes work: a full-size PNG costs ~99 ms to decode here,
/// re-decoded on every library visit. Sixty of them = six seconds, three-core CPU.
/// Re-encoded once at draw size, the same cover decodes in a fraction — and as JPEG,
/// the shell's scaled-decode path takes it (PNG never allowed).
fn shrink_cover(bytes: &[u8]) -> Shrink {
    let small = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()
        .filter(|r| r.format() == Some(image::ImageFormat::Jpeg))
        .and_then(|r| r.into_dimensions().ok())
        .is_some_and(|(w, h)| w <= COVER_MAX_W && h <= COVER_MAX_H);
    if small {
        return Shrink::Keep;
    }
    let Ok(img) = image::load_from_memory(bytes) else {
        return Shrink::Undecodable;
    };
    let img = if img.width() > COVER_MAX_W || img.height() > COVER_MAX_H {
        img.resize(COVER_MAX_W, COVER_MAX_H, image::imageops::FilterType::Triangle)
    } else {
        img
    };
    let mut out = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(std::io::Cursor::new(&mut out), COVER_QUALITY);
    // `into_rgb8`, not `to_rgb8`: `img` is owned and dead after this, so the buffer moves
    // instead of being copied.
    match img.into_rgb8().write_with_encoder(encoder) {
        Ok(()) => Shrink::Shrunk(out),
        // It decoded; only the re-encode failed. The original is still a picture.
        Err(_) => Shrink::Keep,
    }
}

/// Per-host cover cache accounting; scans on `new()` and when the budget is exceeded.
pub(crate) struct CoverCache {
    dir: PathBuf,
    total: u64,
}

impl CoverCache {
    pub(crate) fn new(host: &str, port: u16) -> Self {
        let dir = cache_dir(host, port);
        let _ = std::fs::create_dir_all(&dir);
        let total = prune_cache(&dir);
        Self { dir, total }
    }

    /// This host's cached ENCODED cover bytes for `game_id`, if the cache holds them.
    pub(crate) fn cached(&mut self, game_id: &str) -> Option<Vec<u8>> {
        let path = self.dir.join(cache_name(game_id));
        let bytes = std::fs::read(&path).ok().filter(|b| !b.is_empty())?;
        match shrink_cover(&bytes) {
            Shrink::Keep => Some(bytes),
            // An entry written by a build that cached full-size covers costs ~99 ms to decode,
            // every visit, forever. Shrink it the first time it is read and this visit is the last.
            Shrink::Shrunk(shrunk) => {
                self.put(&path, &shrunk);
                Some(shrunk)
            }
            // Kept by a build that cached whatever came back. Served, it would be a miss that never
            // refetches; dropped, the next fetch can replace it.
            Shrink::Undecodable => {
                if std::fs::remove_file(&path).is_ok() {
                    self.total = self.total.saturating_sub(bytes.len() as u64);
                }
                None
            }
        }
    }

    /// Caches a fetched cover and returns the bytes to hand the shell: the normalised ones the
    /// cache now holds, so a first visit decodes the same small JPEG every later visit does.
    /// `None` for bytes this build cannot decode — nothing to cache, and nothing worth showing.
    pub(crate) fn store(&mut self, game_id: &str, bytes: Vec<u8>) -> Option<Vec<u8>> {
        let bytes = match shrink_cover(&bytes) {
            Shrink::Keep => bytes,
            Shrink::Shrunk(shrunk) => shrunk,
            Shrink::Undecodable => return None,
        };
        self.put(&self.dir.join(cache_name(game_id)), &bytes);
        Some(bytes)
    }

    /// Writes one entry and keeps `total` in step, pruning once it runs over budget.
    fn put(&mut self, path: &std::path::Path, bytes: &[u8]) {
        let old_len = path.metadata().map_or(0, |meta| meta.len());
        if write_cover(path, bytes) {
            self.total = self.total.saturating_sub(old_len).saturating_add(bytes.len() as u64);
            if self.total > CACHE_BUDGET {
                self.total = prune_cache(&self.dir);
            }
        }
    }
}

/// Atomic replacement prevents interrupted writes from poisoning the cache.
fn write_cover(path: &std::path::Path, bytes: &[u8]) -> bool {
    crate::services::atomic::write_parts(path, &[bytes], "cover cache").is_ok()
}

/// Whether a cache entry is stale: a `.tmp` left by a kill mid-write, or the decoded `.raw` and
/// `.hero` art older builds kept beside the covers. `cache_name` leaves only ASCII
/// alphanumerics in an id, so a marker can never be part of one.
fn is_stale(path: &std::path::Path) -> bool {
    path.file_name()
        .map(|n| n.to_string_lossy())
        .is_some_and(|n| n.ends_with(".tmp") || n.ends_with(".raw") || n.ends_with(".hero"))
}

/// Holds one host's cache inside [`CACHE_BUDGET`], oldest file first, and reports what it
/// occupies afterwards.
///
/// The quota is per host directory, so a host with a huge library cannot evict another host's
/// art — and forgetting a host ([`reconcile_host_caches`]) reclaims exactly its own share.
/// Eviction is by write time, not use time.
fn prune_cache(dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        if is_stale(&path) {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let Ok(meta) = path.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        files.push((modified, meta.len(), path));
    }
    files.sort_by_key(|(modified, _, _)| *modified);
    let mut total: u64 = files.iter().map(|(_, len, _)| len).sum();
    if total > CACHE_BUDGET {
        for (_, len, path) in &files {
            if total <= CACHE_BUDGET {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                total -= len;
            }
        }
        tracing::debug!("art: pruned cache in {} to {total} bytes", dir.display());
    }
    total
}
