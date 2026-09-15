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
