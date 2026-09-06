//! On-demand cover-art loading with disk cache (not all-at-once, which caused OOM).
//! Fetches via mTLS, decodes with the pure-Rust `image` crate, handed to the UI as pixels.
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use crate::services::library::GameEntry;

/// A decoded wide hero image, RGB565 little-endian — it goes to the GPU as a raw texture
/// (`Compositor::upload_raw`) rather than through a `Painter`, since nothing is ever
/// rasterized on top of it.
///
/// Half the bytes of RGBA8 on disk, in RAM and over the upload, for an image that is only
/// ever shown full-screen behind a black scrim ([`crate::app::hero::HERO_SCRIM_ALPHA`]) and
/// carries no alpha of its own — at ~1280 wide that is ~1.3 MB rather than ~2.7 MB per
/// hero, and twice as many fit the same disk budget. RGB565 is a native GLES2 texture
/// format, so nothing converts it back on the way in.
pub struct HeroImage {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// RGB8 → RGB565 little-endian, in place. Truncation rather than dithering: the image is dimmed
/// and slowly panning, which is exactly the case where 5/6-bit banding does not read.
///
/// In place (2 bytes written over every 3 read, then truncated) rather than into a second
/// buffer, so the launch path never holds the source and the result at once.
fn to_rgb565(buf: &mut Vec<u8>) {
    let pixels = buf.len() / 3;
    for i in 0..pixels {
        let px = &buf[i * 3..i * 3 + 3];
        let v = (u16::from(px[0] & 0xf8) << 8) | (u16::from(px[1] & 0xfc) << 3) | (u16::from(px[2]) >> 3);
        buf[i * 2..i * 2 + 2].copy_from_slice(&v.to_le_bytes());
    }
    buf.truncate(pixels * 2);
}

/// A decoded cover, straight-alpha RGBA8 at card size.
pub struct CardArt {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// One decoded image, ready to composite.
pub enum ArtLoaded {
    /// Grid cover, stretched to card size.
    Card { game_id: String, art: CardArt },
    /// Wide art for the connecting screen.
    Hero { game_id: String, image: HeroImage },
}

/// Which variant of a game's art a request wants.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ArtKind {
    Card,
    Hero,
}

/// Number of `ArtKind` variants — the width of `ArtLoader::requested`.
const ART_KINDS: usize = 2;

/// An image the UI wants soon.
struct ArtRequest {
    game_id: String,
    kind: ArtKind,
    /// Candidate art paths (host-relative or external URL), tried in order — a
    /// host-reported path can 404 even when another variant works fine.
    paths: Vec<String>,
}

/// Grid card portrait aspect (cropped to avoid distortion).
const TARGET_ART_ASPECT: f32 = 3.0 / 4.0;
/// Max decoded hero width. Full-screen art, so far larger than a card — but deliberately
/// under 1080p width and left for the GPU to upscale: it is a dimmed, moving backdrop, and
/// every extra pixel is resize time on the launch path plus memory for one transient
/// screen. Source heroes are often 3840 wide, so this is where most of the cost is.
const MAX_HERO_WIDTH: u32 = 1280;
/// Hero crop aspect. Deliberately wider than any panel (16:9 at its widest), because
/// the slack between the two is exactly what the connecting screen's pan travels.
const HERO_ASPECT: f32 = 2.4;

/// The center-crop to `aspect` as `(x, y, w, h)`, the whole image if it is already close.
/// A rectangle rather than a cropped image on purpose — see [`crop_and_resize`].
fn crop_rect(w: u32, h: u32, aspect: f32) -> (u32, u32, u32, u32) {
    let current = w as f32 / h as f32;
    if w == 0 || h == 0 || (current - aspect).abs() < 0.01 {
        return (0, 0, w, h);
    }
    if current > aspect {
        let new_w = ((h as f32 * aspect).round() as u32).clamp(1, w);
        ((w - new_w) / 2, 0, new_w, h)
    } else {
        let new_h = ((w as f32 / aspect).round() as u32).clamp(1, h);
        (0, (h - new_h) / 2, w, new_h)
    }
}

/// Center-crops to `aspect` and resamples to whatever `fit` asks for, in one pass.
///
/// The crop is a *view* (`imageops::crop_imm` borrows; `DynamicImage::crop_imm` copies) and the
/// resample reads through it straight into the final buffer. The three full-size copies this
/// replaces — crop, resize, then convert — cost ~100 MB of memcpy on a 4K hero, on a chip where
/// that is the difference between a hero landing before its launch and after it.
///
/// Over a concrete `ImageBuffer`, never a `DynamicImage`: `Triangle` reads each source pixel
/// several times, and every read of a `DynamicImage` is an enum match plus a pixel-format
/// conversion. Converting once up front and sampling the result is the same work done once
/// instead of per filter tap.
///
/// `fit` is given the cropped size and returns the target, so a card can ask for its exact
/// tile size (one resample instead of the old fit-to-480-then-stretch-to-card two) and a hero
/// can bound its width while keeping its aspect.
fn crop_and_resize<P>(
    img: &image::ImageBuffer<P, Vec<u8>>,
    aspect: f32,
    fit: impl FnOnce(u32, u32) -> (u32, u32),
) -> Option<image::ImageBuffer<P, Vec<u8>>>
where
    P: image::Pixel<Subpixel = u8> + 'static,
{
    let (x, y, cw, ch) = crop_rect(img.width(), img.height(), aspect);
    let (tw, th) = fit(cw, ch);
    if cw == 0 || ch == 0 || tw == 0 || th == 0 {
        return None;
    }
    let view = image::imageops::crop_imm(img, x, y, cw, ch);
    Some(if (cw, ch) == (tw, th) {
        view.to_image()
    } else {
        // Through the `Deref`: it is the inner view, not the `SubImage` handle, that is a
        // `GenericImageView`.
        image::imageops::resize(&*view, tw, th, image::imageops::FilterType::Triangle)
    })
}

/// A hero's decoded pixels: cropped to [`HERO_ASPECT`], bounded to [`MAX_HERO_WIDTH`], RGB565.
///
/// Resampled as RGB, not RGBA — the alpha would be thrown away by [`to_rgb565`] anyway, so
/// filtering it is a quarter of the resize's work spent on a channel nothing reads.
fn decode_hero(img: image::DynamicImage) -> Option<HeroImage> {
    let rgb = crop_and_resize(&img.into_rgb8(), HERO_ASPECT, |cw, ch| {
        if cw <= MAX_HERO_WIDTH {
            (cw, ch)
        } else {
            // Integer math, so the bounded size keeps the crop's aspect exactly.
            let h = (u64::from(ch) * u64::from(MAX_HERO_WIDTH) / u64::from(cw)).max(1) as u32;
            (MAX_HERO_WIDTH, h)
        }
    })?;
    let (width, height) = rgb.dimensions();
    let mut pixels = rgb.into_raw();
    to_rgb565(&mut pixels);
    Some(HeroImage { width, height, pixels })
}

/// A cover's decoded pixels: cropped to [`TARGET_ART_ASPECT`] and resampled straight to card
/// size.
fn decode_card(img: image::DynamicImage, card_w: u32, card_h: u32) -> Option<CardArt> {
    let rgba = crop_and_resize(&img.into_rgba8(), TARGET_ART_ASPECT, |_, _| (card_w, card_h))?;
    Some(CardArt {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

/// Cache version magic ("PFR2" — bumped for center-cropped art).
const RAW_CACHE_MAGIC: u32 = 0x50465232;
/// Hero decoded-pixel cache magic ("PFH2" — bumped for RGB565 pixels).
const HERO_CACHE_MAGIC: u32 = 0x50464832;
/// Bytes per pixel a magic's payload is in — heroes RGB565, cards premultiplied RGBA8. Derived
/// from the magic rather than passed alongside it, since the magic is exactly the statement of
/// which pixel convention a file was written with.
fn raw_bpp(magic: u32) -> usize {
    if magic == HERO_CACHE_MAGIC {
        2
    } else {
        4
    }
}
/// Filename markers, appended in this order — see [`cache_path`].
const HERO_SUFFIX: &str = ".hero";
const RAW_SUFFIX: &str = ".raw";

/// Per-host disk budget for card art (encoded bytes plus the decoded `.raw`). A cover's `.raw`
/// is one card's worth of RGBA — a few hundred KB — so an unbounded cache runs to ~100 MB for a
/// 200-game library, on a TV with no disk to spare. At this budget well over a hundred covers
/// stay resident, far more than the grid window, and a miss costs one LAN fetch.
const CARD_CACHE_BUDGET: u64 = 56 * 1024 * 1024;
/// Per-host disk budget for hero art. ~2.4 MB per hero (a ~1.3 MB decoded `.hero.raw` at
/// [`MAX_HERO_WIDTH`] in RGB565 plus the encoded bytes beside it), so this keeps a couple of
/// dozen. Its own budget rather than a share of the card one because the two compete on nothing: a
/// full grid of covers must not evict the hero of the game being launched, and one launch
/// must not evict the visible grid. Sized against *browsing* rather than launching, since a
/// hero is prefetched for every card the focus settles on: at ~6 entries, scrolling one shelf
/// evicted the hero of the game the user then launched.
const HERO_CACHE_BUDGET: u64 = 48 * 1024 * 1024;

fn cache_budget(kind: ArtKind) -> u64 {
    match kind {
        ArtKind::Card => CARD_CACHE_BUDGET,
        ArtKind::Hero => HERO_CACHE_BUDGET,
    }
}

fn cache_root() -> PathBuf {
    crate::services::paths::app_dir().join("art-cache")
}

fn cache_name(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// A host's cache directory name. Its own function because [`sweep_orphan_caches`] matches
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
/// The filesystem work runs on its own thread: the caller is either the startup path or a
/// keypress, and unlinking a stale host's quota is up to ~190 files.
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

/// Cache path for one image: the sanitized id, then a hero marker, then a `.raw` marker for the
/// decoded-pixel copy (`raw = false` is the encoded bytes the host served). The single owner of
/// the naming rule — [`cache_class`] reads it back off the filename.
///
/// Heroes are marked so a game can cache a cover and a hero side by side.
/// This host's cached ENCODED cover bytes for `game_id`, if the cache holds them.
///
/// Shared with the gamepad shell's own art thread, which decodes to a different format than
/// this loader does and so cannot use the `.raw` beside it. One cache either way: whichever UI
/// browsed last warms the other, and the same per-host budget and orphan sweep bound both.
pub(crate) fn cached_cover(host: &str, port: u16, game_id: &str) -> Option<Vec<u8>> {
    let path = cache_path(&cache_dir(host, port), game_id, ArtKind::Card, false);
    std::fs::read(path).ok().filter(|b| !b.is_empty())
}

/// Store encoded cover bytes, then bring the host's directory back inside its budget.
/// Best-effort throughout: a cache that cannot be written costs a re-fetch, nothing more.
pub(crate) fn store_cover(host: &str, port: u16, game_id: &str, bytes: &[u8]) {
    let dir = cache_dir(host, port);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let path = cache_path(&dir, game_id, ArtKind::Card, false);
    let tmp = path.with_extension("tmp");
    // Write-then-rename, like the raw path above: a kill mid-write must not leave a truncated
    // file that later reads as a cover.
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    prune_cache(&dir);
}

fn cache_path(dir: &std::path::Path, game_id: &str, kind: ArtKind, raw: bool) -> PathBuf {
    let hero = if matches!(kind, ArtKind::Hero) { HERO_SUFFIX } else { "" };
    let raw = if raw { RAW_SUFFIX } else { "" };
    dir.join(format!("{}{hero}{raw}", cache_name(game_id)))
}

/// Write-then-rename (prevents truncated cache files on kill mid-write). Header and pixels go
/// as two writes so a full-image copy isn't made just to prepend 12 bytes. Best-effort: a cache
/// that can't be written costs a re-fetch, nothing more.
/// Returns the bytes the file now occupies — zero if the write failed, so a full disk can't
/// inflate the caller's running total into pruning on every write ([`prune_cache`]).
fn write_raw(path: &std::path::Path, magic: u32, width: u32, height: u32, pixels: &[u8]) -> u64 {
    let mut header = [0u8; 12];
    header[0..4].copy_from_slice(&magic.to_le_bytes());
    header[4..8].copy_from_slice(&width.to_le_bytes());
    header[8..12].copy_from_slice(&height.to_le_bytes());
    match crate::services::atomic::write_parts(path, &[&header, pixels], "art raw cache") {
        Ok(()) => (header.len() + pixels.len()) as u64,
        Err(_) => 0,
    }
}

/// Read raw cache, if present and written with this magic (and so this pixel convention —
/// card pixels are premultiplied, hero pixels straight, and the two must never be
/// mistaken for each other).
///
/// Header first, then the payload into its own exactly-sized buffer: the pixels are all but
/// 12 bytes of the file, so reading the lot and shifting it down over the header would be a
/// multi-MB memmove — and a hero is read this way on the launch path (`cached_hero`). A
/// mismatched magic or size costs only the header read.
fn read_raw(path: &std::path::Path, magic: u32) -> Option<(u32, u32, Vec<u8>)> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).ok()?;
    if u32::from_le_bytes(header[0..4].try_into().ok()?) != magic {
        return None;
    }
    let width = u32::from_le_bytes(header[4..8].try_into().ok()?);
    let height = u32::from_le_bytes(header[8..12].try_into().ok()?);
    let len = (width as usize)
        .checked_mul(height as usize)?
        .checked_mul(raw_bpp(magic))?;
    // Against the file's own length, before allocating for it: a truncated (or absurd)
    // header must not turn into a multi-MB reservation.
    if file.metadata().ok()?.len() != (len + 12) as u64 {
        return None;
    }
    let mut pixels = Vec::with_capacity(len);
    file.read_to_end(&mut pixels).ok()?;
    (pixels.len() == len).then_some((width, height, pixels))
}

/// A hero straight out of the decoded-pixel cache, or `None` if it isn't cached (or was
/// written by an older build). One file read, no decode and no fetch, which is what makes it
/// safe to call on the UI thread.
fn read_hero_raw(path: &std::path::Path) -> Option<HeroImage> {
    let (width, height, pixels) = read_raw(path, HERO_CACHE_MAGIC)?;
    Some(HeroImage { width, height, pixels })
}

/// Which budget a cached file counts against — [`cache_path`]'s markers, read back off the
/// filename in the reverse of the order they were appended. `None` for a staging file, which no
/// budget owns: a `.tmp` left behind by a kill mid-write is deleted outright by [`prune_cache`].
///
/// `cache_name` leaves only ASCII alphanumerics in the id itself, so a marker can never be part
/// of one. Anything unrecognized counts as card art, the smaller of the two to be wrong about.
fn cache_class(path: &std::path::Path) -> Option<ArtKind> {
    let name = path.file_name()?.to_string_lossy();
    if name.ends_with(".tmp") {
        return None;
    }
    let stem = name.strip_suffix(RAW_SUFFIX).unwrap_or(&name);
    Some(if stem.ends_with(HERO_SUFFIX) {
        ArtKind::Hero
    } else {
        ArtKind::Card
    })
}

/// Bytes a host's cache directory holds, per [`ArtKind`] — what a caller adds its own writes to
/// so it can tell when the directory is worth walking again.
type CacheTotals = [u64; ART_KINDS];

/// Holds one host's cache inside its per-kind budget, oldest file first, and reports what each
/// kind occupies afterwards.
///
/// The quota is per host directory, so a host with a huge library cannot evict another host's
/// art — and forgetting a host ([`reconcile_host_caches`]) reclaims exactly its own share.
///
/// Eviction is by write time, not use time: nothing here touches a file it serves from cache, and
/// an extra `utimes` per grid card is not worth the sharper policy. So a re-fetch after eviction
/// is possible for art the user is still looking at, which costs one LAN request.
fn prune_cache(dir: &std::path::Path) -> CacheTotals {
    let mut totals: CacheTotals = [0; ART_KINDS];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return totals;
    };
    let mut files: Vec<(ArtKind, std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for path in entries.flatten().map(|e| e.path()) {
        let Some(kind) = cache_class(&path) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let Ok(meta) = path.metadata() else { continue };
        let Ok(modified) = meta.modified() else { continue };
        files.push((kind, modified, meta.len(), path));
    }
    files.sort_by_key(|(_, modified, _, _)| *modified);

    for (kind, _, len, _) in &files {
        totals[*kind as usize] += len;
    }
    for kind in [ArtKind::Card, ArtKind::Hero] {
        let budget = cache_budget(kind);
        let total = &mut totals[kind as usize];
        if *total <= budget {
            continue;
        }
        for (_, _, len, path) in files.iter().filter(|(k, ..)| *k == kind) {
            if *total <= budget {
                break;
            }
            if std::fs::remove_file(path).is_ok() {
                *total -= len;
            }
        }
        tracing::debug!("art: pruned {:?} cache in {} to {total} bytes", kind, dir.display());
    }
    totals
}

/// Stretches a cached cover to card size, or hands it back untouched when it is already that
/// size — which is the normal case, since covers are cached at card size. Only a `.raw` written
/// by an older build, or by a session whose panel gave a different card size, is resampled here.
pub(crate) fn resize_card(src: CardArt, w: u32, h: u32) -> CardArt {
    if (src.width, src.height) == (w, h) || src.width == 0 || src.height == 0 || w == 0 || h == 0 {
        return src;
    }
    let Some(img) = image::RgbaImage::from_raw(src.width, src.height, src.pixels) else {
        return CardArt {
            width: 0,
            height: 0,
            pixels: Vec::new(),
        };
    };
    let resized = image::imageops::resize(&img, w, h, image::imageops::FilterType::Triangle);
    CardArt {
        width: w,
        height: h,
        pixels: resized.into_raw(),
    }
}

/// `worker`'s fixed, per-host config — bundled to keep its arg count sane.
struct WorkerConfig {
    host: String,
    query_port: u16,
    identity: (String, String),
    fingerprint: Option<[u8; 32]>,
    dir: PathBuf,
    card_w: u32,
    card_h: u32,
}

/// Background fetcher/decoder. Requests go in, decoded covers come out; both ends are
/// non-blocking for the UI thread.
pub struct ArtLoader {
    tx: Sender<ArtRequest>,
    rx: Receiver<ArtLoaded>,
    /// Ids already handed to the worker, so scrolling over the same card repeatedly doesn't
    /// queue it repeatedly. Kept per kind (indexed by `ArtKind as usize`) because focus moving
    /// back and forth over a card must not re-queue its much larger hero either, and the two
    /// are forgotten independently.
    requested: [HashSet<String>; ART_KINDS],
    /// This host's cache directory, so [`Self::cached_hero`] can read it without going
    /// through the worker — the worker's own copy is in its `WorkerConfig`.
    dir: PathBuf,
}

impl ArtLoader {
    /// Spawn loader. `query_port` is what's dialed (separate from identity `port`).
    /// Card dimensions determine cover stretch-to size.
    pub fn spawn(
        host: String,
        port: u16,
        query_port: u16,
        identity: (String, String),
        fingerprint: Option<[u8; 32]>,
        (card_w, card_h): (u32, u32),
    ) -> Self {
        let (tx_req, rx_req) = std::sync::mpsc::channel::<ArtRequest>();
        let (tx_done, rx_done) = std::sync::mpsc::channel::<ArtLoaded>();
        let dir = cache_dir(&host, port);
        let config = WorkerConfig {
            host,
            query_port,
            identity,
            fingerprint,
            dir: dir.clone(),
            card_w,
            card_h,
        };
        std::thread::Builder::new()
            .name("punktfunk-webos-art".into())
            .spawn(move || worker(&config, &rx_req, &tx_done))
            .expect("spawn art-loader thread");
        Self {
            tx: tx_req,
            rx: rx_done,
            requested: Default::default(),
            dir,
        }
    }

    /// Queues one request unless `game_id` has already been asked for in `kind`. The id is
    /// remembered even when there are no candidate paths: a game with no art at all must not
    /// be re-queued every frame forever.
    ///
    /// `paths` is a closure so the already-requested case — the overwhelmingly common one, since
    /// these are called for the whole prefetch window every frame — builds no path strings at all.
    fn queue(&mut self, game_id: &str, kind: ArtKind, paths: impl FnOnce() -> Vec<String>) {
        let requested = &mut self.requested[kind as usize];
        // Membership before insert: an already-requested id shouldn't pay for an allocation
        // just to be looked up.
        if requested.contains(game_id) {
            return;
        }
        requested.insert(game_id.to_string());
        let paths = paths();
        if paths.is_empty() {
            return;
        }
        // A closed channel means the worker is gone; the card keeps its placeholder.
        let _ = self.tx.send(ArtRequest {
            game_id: game_id.to_string(),
            kind,
            paths,
        });
    }

    /// Asks for `game`'s cover if it hasn't been asked for already. Cheap enough to call
    /// for every card in the prefetch window every frame.
    pub fn request(&mut self, game: &GameEntry) {
        self.queue(&game.id, ArtKind::Card, || {
            // Preference order: portrait (right aspect), then header, then hero.
            [
                game.art.portrait.as_deref(),
                game.art.header.as_deref(),
                game.art.hero.as_deref(),
            ]
            .into_iter()
            .flatten()
            .map(str::to_string)
            .collect()
        });
    }

    /// Asks for `game`'s wide hero art (the connecting screen's backdrop) if it hasn't
    /// been asked for already. Called for the focused card, so the image is usually
    /// decoded and waiting by the time the user actually launches.
    ///
    /// Portrait art is deliberately not a fallback here: cropped to a hero's aspect
    /// there'd be almost nothing left of it, and the connecting screen falls back to
    /// its plain black fade perfectly well.
    pub fn request_hero(&mut self, game: &GameEntry) {
        self.queue(&game.id, ArtKind::Hero, || {
            [game.art.hero.as_deref(), game.art.header.as_deref()]
                .into_iter()
                .flatten()
                .map(str::to_string)
                .collect()
        });
    }

    /// `game_id`'s hero out of the disk cache, on the calling thread. For the launch itself:
    /// going through the worker for an image that is already decoded on disk would put it
    /// behind whatever card art that thread is mid-fetch on, which is how a hero that *was*
    /// cached still managed to arrive after the hand-off.
    ///
    /// A hit counts as a request, so nothing queues the same image a second time.
    pub fn cached_hero(&mut self, game_id: &str) -> Option<HeroImage> {
        let image = read_hero_raw(&cache_path(&self.dir, game_id, ArtKind::Hero, true))?;
        self.requested[ArtKind::Hero as usize].insert(game_id.to_string());
        Some(image)
    }

    /// Forgets that `game_id`'s hero was requested, so it can be asked for again. Needed
    /// when a hero arrives too late to be of use and is dropped — without this the game
    /// would never get another chance at one, even though its bytes are now cached.
    pub fn forget_hero(&mut self, game_id: &str) {
        self.requested[ArtKind::Hero as usize].remove(game_id);
    }

    /// Forgets that `game_id` was requested, so a later scroll back re-requests it. Served
    /// from the disk cache, so this costs a decode rather than a round-trip.
    pub fn forget(&mut self, game_id: &str) {
        self.requested[ArtKind::Card as usize].remove(game_id);
    }

    /// Drains everything decoded since the last call.
    pub fn drain(&self) -> Vec<ArtLoaded> {
        self.rx.try_iter().collect()
    }
}

fn worker(config: &WorkerConfig, rx: &Receiver<ArtRequest>, tx: &Sender<ArtLoaded>) {
    let &WorkerConfig {
        ref host,
        query_port,
        ref identity,
        fingerprint,
        ref dir,
        card_w,
        card_h,
    } = config;
    let _ = std::fs::create_dir_all(dir);
    // Once at startup: the budgets are also enforced here, so a directory left over an upgrade
    // (or by a kill before the write-triggered prune below) is brought inside them without
    // waiting for this session to write anything.
    // What the directory holds, carried forward across writes and only re-derived when a
    // budget is actually crossed. Pruning walks the directory and `stat`s every file in it, so
    // doing it per write put a `read_dir` storm on both the grid's fetches and the launch path.
    let mut totals = prune_cache(dir);
    // One transport reused for every fetch, so the connection (and, for punktfunk, the
    // client-cert handshake) is paid once rather than per cover — real avoidable cost that
    // scales with library size. Built lazily, so a fully cached library never connects at all.
    let mut fetcher = None;

    // Local queue rather than straight `recv()`: a hero request gates the connecting
    // screen, so it has to jump whatever card-art backlog the grid has just queued. A
    // closed channel means the host was switched away from, and the worker is done.
    let mut queue: VecDeque<ArtRequest> = VecDeque::new();
    loop {
        if queue.is_empty() {
            match rx.recv() {
                Ok(req) => queue.push_back(req),
                Err(_) => return,
            }
        }
        while let Ok(req) = rx.try_recv() {
            queue.push_back(req);
        }
        let at = queue.iter().position(|r| r.kind == ArtKind::Hero).unwrap_or_default();
        let Some(req) = queue.remove(at) else { continue };

        // Decoded-pixel cache. Worth far more for a hero than for a card: decoding a
        // full-size hero JPEG on this SoC takes long enough to miss the launch it was
        // fetched for, so the encoded-bytes layer below is not enough on its own.
        let raw_cached = cache_path(dir, &req.game_id, req.kind, true);
        let from_raw_cache = match req.kind {
            ArtKind::Card => read_raw(&raw_cached, RAW_CACHE_MAGIC)
                .filter(|(width, height, pixels)| pixels.len() == (*width as usize) * (*height as usize) * 4)
                .map(|(width, height, pixels)| ArtLoaded::Card {
                    game_id: req.game_id.clone(),
                    art: resize_card(CardArt { width, height, pixels }, card_w, card_h),
                }),
            ArtKind::Hero => read_hero_raw(&raw_cached).map(|image| ArtLoaded::Hero {
                game_id: req.game_id.clone(),
                image,
            }),
        };
        if let Some(loaded) = from_raw_cache {
            if tx.send(loaded).is_err() {
                return;
            }
            continue;
        }

        let cached = cache_path(dir, &req.game_id, req.kind, false);
        let bytes = match std::fs::read(&cached) {
            Ok(b) if !b.is_empty() => b,
            _ => {
                if fetcher.is_none() {
                    match crate::services::library::agent(identity, fingerprint) {
                        Ok(a) => fetcher = Some(a),
                        Err(e) => {
                            tracing::warn!("art: {} opening art transport failed: {e}", req.game_id);
                            continue;
                        }
                    }
                }
                let Some(agent) = fetcher.as_ref() else { continue };
                let mut fetched = None;
                for path in &req.paths {
                    match crate::services::library::fetch_art(agent, host, query_port, path) {
                        Ok(b) => {
                            fetched = Some(b);
                            break;
                        }
                        Err(e) => tracing::warn!("art: {} fetch {} failed: {e}", req.game_id, path),
                    }
                }
                let Some(fetched) = fetched else {
                    continue;
                };
                // Write-then-rename, never truncate-in-place: a kill mid-write would
                // otherwise leave a truncated file that gets served from cache forever.
                if crate::services::atomic::write_parts(&cached, &[&fetched], "art bytes cache").is_ok() {
                    totals[req.kind as usize] += fetched.len() as u64;
                }
                fetched
            }
        };

        let decoded = match image::load_from_memory(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("art: {} decode failed ({} bytes): {e}", req.game_id, bytes.len());
                // Drop a cache entry that won't decode — otherwise it poisons this card for
                // the life of the install.
                let _ = std::fs::remove_file(&cached);
                continue;
            }
        };
        // Decoded straight to the size that will be used, and cached that way: the crop and the
        // stretch are the same every time, so paying them once here saves a full resample on
        // every later cache hit — and a card-sized `.raw` is smaller, so more covers fit the
        // same budget.
        let loaded = match req.kind {
            ArtKind::Hero => {
                let Some(image) = decode_hero(decoded) else {
                    tracing::warn!("art: {} hero decoded to zero size", req.game_id);
                    continue;
                };
                totals[ArtKind::Hero as usize] +=
                    write_raw(&raw_cached, HERO_CACHE_MAGIC, image.width, image.height, &image.pixels);
                ArtLoaded::Hero {
                    game_id: req.game_id,
                    image,
                }
            }
            ArtKind::Card => {
                let Some(art) = decode_card(decoded, card_w, card_h) else {
                    tracing::warn!("art: {} card decode failed ({card_w}x{card_h})", req.game_id);
                    continue;
                };
                totals[ArtKind::Card as usize] +=
                    write_raw(&raw_cached, RAW_CACHE_MAGIC, art.width, art.height, &art.pixels);
                ArtLoaded::Card {
                    game_id: req.game_id,
                    art,
                }
            }
        };
        if totals[req.kind as usize] > cache_budget(req.kind) {
            totals = prune_cache(dir);
        }
        if tx.send(loaded).is_err() {
            return;
        }
    }
}
