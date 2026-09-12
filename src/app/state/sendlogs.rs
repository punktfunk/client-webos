//! Sends the session log to a paired host, from either UI's host menu. The upload runs on a
//! worker: the pointer UI reports it on the Home status bar, the console as a notice.
use crate::app::App;
use crate::core::screen::Screen;
use crate::services::library::{self, LibraryError};
use std::path::Path;
use std::sync::mpsc::TryRecvError;

/// The tail of the log that travels; the file itself rotates at this size too.
const MAX_LOG_BYTES: u64 = 960 * 1024;

impl App {
    /// The host menu's "Send logs to host": closes the menu and uploads to sidebar entry `idx`.
    pub(crate) fn send_logs_to_host(&mut self, idx: usize) {
        let target = self
            .hosts
            .entries
            .get(idx)
            .and_then(|e| self.known_host(e.host(), e.port()))
            .and_then(|known| {
                Some(HostTarget {
                    name: known.name.clone(),
                    addr: known.addr.clone(),
                    mgmt_port: known.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
                    identity: self.identity.clone(),
                    pin: known.fingerprint()?,
                })
            });
        self.screens.host_menu_index = None;
        self.nav.screen = Screen::Home;
        let Some(target) = target else { return };
        self.set_home_status(Some(format!("Sending logs to {}…", target.name)), false);
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.send_logs = Some(rx);
        std::thread::spawn(move || {
            let _ = tx.send(upload_to_host(&target));
        });
    }

    /// Drain the upload worker's result, if it has landed — called each tick
    /// alongside the other `drain_*`s. Returns whether anything changed.
    pub(crate) fn drain_send_logs(&mut self) -> bool {
        let Some(rx) = &self.jobs.send_logs else { return false };
        match rx.try_recv() {
            Ok(Ok(s) | Err(s)) => {
                self.set_home_status(Some(s), false);
                self.jobs.send_logs = None;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                self.jobs.send_logs = None;
                false
            }
        }
    }
}

/// Host endpoint and credentials resolved before starting the worker.
/// Deliberately omits `Debug` because `identity` contains private key material.
pub(crate) struct HostTarget {
    pub(crate) name: String,
    pub(crate) addr: String,
    pub(crate) mgmt_port: u16,
    pub(crate) identity: (String, String),
    pub(crate) pin: [u8; 32],
}

/// Reads the newest [`MAX_LOG_BYTES`], dropping any partial leading line.
fn log_tail(path: &Path) -> Result<String, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let unreadable = |e: std::io::Error| format!("Couldn't read the log file: {e}");
    let mut f = std::fs::File::open(path).map_err(unreadable)?;
    let len = f.metadata().map_err(unreadable)?.len();
    let truncated = len > MAX_LOG_BYTES;
    if truncated {
        f.seek(SeekFrom::End(-(MAX_LOG_BYTES as i64))).map_err(unreadable)?;
    }
    let mut raw = Vec::with_capacity(len.min(MAX_LOG_BYTES) as usize);
    f.read_to_end(&mut raw).map_err(unreadable)?;
    // The seek may land within a UTF-8 character, so conversion remains lossy.
    let from = if truncated {
        raw.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)
    } else {
        0
    };
    let text = String::from_utf8_lossy(&raw[from..]);
    if text.trim().is_empty() {
        return Err("No logs to send yet.".into());
    }
    Ok(if truncated {
        format!("… older log lines truncated …\n{text}")
    } else {
        text.into_owned()
    })
}

/// Posts the newest log tail to `target` as plain text on the paired mTLS identity. Blocks, so
/// call it from a worker. Either side of the result is the status line to show.
pub(crate) fn upload_to_host(target: &HostTarget) -> Result<String, String> {
    post_log(target)
        .inspect(|s| tracing::info!("send logs: {s}"))
        .inspect_err(|e| tracing::warn!("send logs failed: {e}"))
}

fn post_log(target: &HostTarget) -> Result<String, String> {
    let path = crate::logger::latest_log_file(&crate::services::store::app_dir()).ok_or("No logs to send yet.")?;
    let log = log_tail(&path)?;
    let body = format!(
        "punktfunk-webos {} (webos {}) — client log bundle\n{log}",
        crate::core::VERSION,
        std::env::consts::ARCH,
    );
    let agent = library::agent(&target.identity, Some(target.pin))
        .map_err(|e| format!("Couldn't send logs to {} — {e}", target.name))?;
    let url = format!(
        "{}/api/v1/client-logs",
        library::base_url(&target.addr, target.mgmt_port)
    );
    match agent
        .post(url.as_str())
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(_) => Ok(format!(
            "Logs sent to {} — download them from its web console's Logs page",
            target.name
        )),
        Err(e) => Err(match library::classify(e) {
            LibraryError::Http(413) => "Log file too large to send (1 MB limit).".into(),
            LibraryError::NotPaired => format!("{} refused the logs — pair with it again.", target.name),
            other => format!(
                "{}: {}",
                target.name,
                crate::app::view::hostpower::refusal_message(&other)
            ),
        }),
    }
}
