//! The write destination — a rotating log file, or a TCP stream to a dev machine.
use crate::core::VERSION;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::launch;

/// Leaves batch-overrun headroom below the host's 1 MiB log-bundle limit.
const MAX_LOG_BYTES: u64 = 960 * 1024;
/// Rotations kept (`base.log.1`..`.3`), bounding disk use at
/// ~`(MAX_LOG_ROTATIONS + 1) * MAX_LOG_BYTES`.
const MAX_LOG_ROTATIONS: usize = 3;

/// Log destination (file or TCP). Non-blocking dispatch prevents blocking video pump.
pub(super) enum Sink {
    File {
        file: std::fs::File,
        written: u64,
        /// Active log path, so a full file can be rotated (renamed) and reopened.
        path: PathBuf,
    },
    Tcp {
        stream: Option<TcpStream>,
        fallback: Box<Self>,
    },
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::File { file, written, .. } => {
                let n = file.write(buf)?;
                *written += n as u64;
                Ok(n)
            }
            Self::Tcp { stream, fallback } => {
                if let Some(socket) = stream {
                    match socket.write(buf) {
                        Ok(n) => return Ok(n),
                        Err(_) => *stream = None,
                    }
                }
                fallback.write(buf)
            }
        }
    }

    /// `tracing_appender`'s worker thread flushes after each drained batch, not per
    /// line — so the size/rotation check runs once per batch instead of per write.
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::File { file, written, path } => {
                file.flush()?;
                if *written >= MAX_LOG_BYTES {
                    rotate(path);
                    *file = open_fresh(path)?;
                    *written = 0;
                }
                Ok(())
            }
            Self::Tcp { stream, fallback } => {
                if let Some(socket) = stream {
                    if socket.flush().is_ok() {
                        return Ok(());
                    }
                    *stream = None;
                }
                fallback.flush()
            }
        }
    }
}

/// Open TCP or file sink; fall back to file if unreachable (dev convenience, not critical).
pub(super) fn open(app_dir: &Path) -> Result<Sink> {
    let fallback = open_file(app_dir)?;
    if let Some(addr) = launch::telemetry_addr() {
        if let Some(stream) = connect_telemetry(addr) {
            return Ok(Sink::Tcp {
                stream: Some(stream),
                fallback: Box::new(fallback),
            });
        }
    }
    Ok(fallback)
}

fn connect_telemetry(addr: &'static str) -> Option<TcpStream> {
    const BUDGET: Duration = Duration::from_millis(500);
    let deadline = Instant::now() + BUDGET;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    // DNS has no portable cancellation API; only this worker may wait on it.
    std::thread::Builder::new()
        .name("telemetry-connect".into())
        .spawn(move || {
            let Ok(addresses) = addr.to_socket_addrs() else { return };
            for address in addresses {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return;
                };
                if remaining.is_zero() {
                    return;
                }
                if let Ok(stream) = TcpStream::connect_timeout(&address, remaining) {
                    if stream.set_write_timeout(Some(BUDGET)).is_ok() {
                        let _ = tx.send(stream);
                    }
                    return;
                }
            }
        })
        .ok()?;
    rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).ok()
}

/// A fresh active log each launch; the previous session rotates to `.1` first, so
/// relaunching to reproduce a bug keeps the prior run.
fn open_file(app_dir: &Path) -> Result<Sink> {
    let path = log_file_path(app_dir);
    if path.metadata().is_ok_and(|m| m.len() > 0) {
        rotate(&path);
    }
    prune_other_builds(app_dir, &path);
    let file = open_fresh(&path).with_context(|| format!("open log file {}", path.display()))?;
    Ok(Sink::File { file, written: 0, path })
}

/// Create (truncating) a fresh active log at `path`.
fn open_fresh(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
}

/// `base.log` → `base.log.<n>`.
fn numbered(base: &Path, n: usize) -> PathBuf {
    let mut s = base.as_os_str().to_owned();
    s.push(format!(".{n}"));
    PathBuf::from(s)
}

/// Shift the ring down one: drop `.MAX`, rename `.k`→`.k+1`, then `base`→`.1`.
/// Best-effort — a failed rename loses one rotation, never the active log.
fn rotate(base: &Path) {
    let _ = std::fs::remove_file(numbered(base, MAX_LOG_ROTATIONS));
    for n in (1..MAX_LOG_ROTATIONS).rev() {
        let _ = std::fs::rename(numbered(base, n), numbered(base, n + 1));
    }
    let _ = std::fs::rename(base, numbered(base, 1));
}

/// Absolute path of the active log file (`open_file`'s target).
fn log_file_path(app_dir: &Path) -> PathBuf {
    app_dir.join(format!("punktfunk-webos-{VERSION}.log"))
}

fn is_log_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((version, suffix)) = name
        .strip_prefix("punktfunk-webos-")
        .and_then(|name| name.split_once(".log"))
    else {
        return false;
    };
    !version.is_empty()
        && (suffix.is_empty()
            || suffix
                .strip_prefix('.')
                .is_some_and(|rotation| rotation.parse::<usize>().is_ok()))
}

/// Whether `path` is `active` or one of its rotations.
fn is_this_build(path: &Path, active: &Path) -> bool {
    let (Some(name), Some(active)) = (
        path.file_name().and_then(|n| n.to_str()),
        active.file_name().and_then(|n| n.to_str()),
    ) else {
        return false;
    };
    name.strip_prefix(active).is_some_and(|rest| {
        rest.is_empty()
            || rest
                .strip_prefix('.')
                .is_some_and(|rotation| rotation.parse::<usize>().is_ok())
    })
}

/// Drops the logs other builds wrote, all but the newest. Every beta build is its own version
/// (`X.Y.Z+git.<sha>`) and [`rotate`] only ever shifts this build's files, so without this each
/// update left up to `(MAX_LOG_ROTATIONS + 1) * MAX_LOG_BYTES` behind on the partition every
/// developer app shares. The newest survives so a crash just before an update can still be sent
/// ([`latest_log_file`] looks across versions).
fn prune_other_builds(app_dir: &Path, active: &Path) {
    let Ok(entries) = std::fs::read_dir(app_dir) else {
        return;
    };
    let mut others: Vec<(PathBuf, std::time::SystemTime)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_log_file(path) && !is_this_build(path, active))
        .map(|path| {
            let modified = path.metadata().and_then(|m| m.modified());
            (path, modified.unwrap_or(std::time::UNIX_EPOCH))
        })
        .collect();
    others.sort_by_key(|(_, modified)| std::cmp::Reverse(*modified));
    for (path, _) in others.into_iter().skip(1) {
        let _ = std::fs::remove_file(path);
    }
}

/// The run before this one (`base.log.1`), if it has any lines. A crash is only ever in here:
/// relaunching to report it rotates the run that crashed out of the active log.
pub fn previous_log_file(app_dir: &Path) -> Option<PathBuf> {
    let path = numbered(&log_file_path(app_dir), 1);
    path.metadata().is_ok_and(|m| m.len() > 0).then_some(path)
}

/// Returns the non-empty active log, otherwise the newest version or rotation.
/// The active log wins because renaming preserves a rotated file's newer mtime.
pub fn latest_log_file(app_dir: &Path) -> Option<PathBuf> {
    let active = log_file_path(app_dir);
    if active.metadata().is_ok_and(|m| m.len() > 0) {
        return Some(active);
    }
    std::fs::read_dir(app_dir)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| is_log_file(&entry.path()))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            (meta.len() > 0).then_some(())?;
            Some((entry.path(), meta.modified().ok()?))
        })
        .max_by_key(|(_, mtime)| *mtime)
        .map(|(path, _)| path)
}

#[cfg(test)]
mod previous_run_tests {
    use super::{log_file_path, numbered, previous_log_file, prune_other_builds};

    /// A relaunch rotates the crashed run to `.1`, and that is the file a report needs.
    #[test]
    fn the_rotated_run_is_found_and_an_empty_one_is_not() {
        let dir = std::env::temp_dir().join(format!("pf-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = numbered(&log_file_path(&dir), 1);
        assert_eq!(previous_log_file(&dir), None, "nothing rotated yet");
        std::fs::write(&first, "").unwrap();
        assert_eq!(previous_log_file(&dir), None, "an empty rotation is no log");
        std::fs::write(&first, "panicked at stream.rs\n").unwrap();
        assert_eq!(previous_log_file(&dir), Some(first));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Other builds' logs go, all but the newest; this build's rotations and unrelated files stay.
    #[test]
    fn other_builds_leave_only_their_last_log() {
        use std::time::{Duration, SystemTime};
        let dir = std::env::temp_dir().join(format!("pf-log-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let aged = |name: &str, secs: u64| {
            let path = dir.join(name);
            std::fs::write(&path, "line\n").unwrap();
            let file = std::fs::File::options().write(true).open(&path).unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
            path
        };
        let oldest = aged("punktfunk-webos-test-older.log.1", 300);
        let older = aged("punktfunk-webos-test-older.log", 200);
        let newest = aged("punktfunk-webos-test-newer.log", 100);
        let active = log_file_path(&dir);
        let ours = aged(numbered(&active, 1).file_name().unwrap().to_str().unwrap(), 400);
        let unrelated = aged("settings.json", 500);
        prune_other_builds(&dir, &active);
        assert!(newest.exists(), "the newest other build's log is kept");
        assert!(!older.exists() && !oldest.exists(), "the rest of other builds' logs go");
        assert!(ours.exists(), "this build's rotations are rotate's business");
        assert!(unrelated.exists(), "only log files are touched");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
