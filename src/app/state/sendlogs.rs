//! Sends the session log to a paired host or, with confirmation, the developer.
//! Uploads run in the background and report through the Home status bar.
//!
//! Rendering lives in `app::view::sendlogs`.
use crate::app::nav::ScreenKey;
use crate::app::App;
use crate::core::event::MenuEvent;
use crate::core::screen::Screen;
use crate::services::library::{self, LibraryError};
use std::path::Path;

/// Upload endpoint (see the Go service: POST multipart `file` field to `/upload`).
const UPLOAD_URL: &str = "https://www.upload.dyptan.dev/upload";
/// The tail of the log that travels; the file itself rotates at this size too.
const MAX_LOG_BYTES: u64 = 960 * 1024;

/// What the background upload thread reports back — a user-facing status line
/// either way, shown in the Home status bar by `drain_send_logs`.
pub(crate) enum SendLogsMsg {
    Ok(String),
    Err(String),
}

impl App {
    /// Resolves a reachable, paired host for log delivery.
    pub(crate) fn send_logs_host(&self) -> Option<HostTarget> {
        let known = self.reachable_selected_host()?;
        let pin = known.fingerprint()?;
        Some(HostTarget {
            name: known.name.clone(),
            addr: known.addr.clone(),
            mgmt_port: known.mgmt_port.unwrap_or(library::DEFAULT_MGMT_PORT),
            identity: self.identity.clone(),
            pin,
        })
    }

    /// Whether "Send logs" would send directly to the host.
    pub(crate) fn send_logs_host_ready(&self) -> bool {
        self.reachable_selected_host()
            .is_some_and(crate::core::model::KnownHost::is_paired)
    }

    /// Sends to the host when available, otherwise opens developer confirmation.
    pub(crate) fn send_logs_action(&mut self) {
        let Some(target) = self.send_logs_host() else {
            self.open_send_logs();
            return;
        };
        let status = format!("Sending logs to {}…", target.name);
        self.spawn_log_upload(status, move |path| upload_to_host(path, &target));
        self.nav.resume(Screen::Home);
    }

    /// Open the confirmation modal, defaulting focus to Cancel.
    pub(crate) fn open_send_logs(&mut self) {
        self.nav.enter(Screen::SendLogs, 1);
    }

    /// Left/Right toggle Cancel/Send; Confirm acts on the focused button. Both
    /// buttons (and Back) close the modal and return to Home — Send additionally
    /// starts the upload.
    pub(crate) fn handle_send_logs_event(&mut self, ev: MenuEvent) {
        if self.confirm_nav_event(ev) {
            return;
        }
        match ev {
            MenuEvent::Confirm => {
                if self.nav.cursor(ScreenKey::SendLogs) == 0 {
                    self.spawn_log_upload("Sending logs to the developer…".into(), upload_logs);
                }
                self.close_send_logs();
            }
            MenuEvent::Back => self.close_send_logs(),
            MenuEvent::Up | MenuEvent::Down | MenuEvent::Secondary | MenuEvent::Left | MenuEvent::Right => {}
        }
    }

    fn close_send_logs(&mut self) {
        // No cursor reset needed: `open_send_logs` enters at Cancel every time.
        self.nav.resume(Screen::Home);
    }

    /// Starts a background upload and publishes its status through `drain_send_logs`.
    fn spawn_log_upload(&mut self, status: String, work: impl FnOnce(&Path) -> SendLogsMsg + Send + 'static) {
        let Some(path) = crate::logger::latest_log_file(&crate::services::store::app_dir()) else {
            self.set_home_status(Some("No logs to send yet.".into()), false);
            return;
        };
        let (tx, rx) = std::sync::mpsc::channel();
        self.jobs.send_logs = Some(rx);
        self.set_home_status(Some(status), false);
        tracing::info!("send logs: uploading {}", path.display());
        std::thread::spawn(move || {
            let _ = tx.send(work(&path));
        });
    }

    /// Drain the upload worker's result, if it has landed — called each tick
    /// alongside the other `drain_*`s. Returns whether anything changed.
    pub(crate) fn drain_send_logs(&mut self) -> bool {
        let Some(rx) = &self.jobs.send_logs else { return false };
        match rx.try_recv() {
            Ok(msg) => {
                match msg {
                    SendLogsMsg::Ok(s) => {
                        tracing::info!("send logs: {s}");
                        self.set_home_status(Some(s), false);
                    }
                    SendLogsMsg::Err(s) => {
                        tracing::warn!("send logs failed: {s}");
                        self.set_home_status(Some(s), false);
                    }
                }
                self.jobs.send_logs = None;
                true
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
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

/// Posts the log tail as plain text using the paired mTLS identity.
fn upload_to_host(path: &Path, target: &HostTarget) -> SendLogsMsg {
    let log = match log_tail(path) {
        Ok(log) => log,
        Err(e) => return SendLogsMsg::Err(e),
    };
    let body = format!(
        "punktfunk-webos {} (webos {}) — client log bundle\n{log}",
        crate::core::VERSION,
        std::env::consts::ARCH,
    );
    let agent = match library::agent(&target.identity, Some(target.pin)) {
        Ok(a) => a,
        Err(e) => return SendLogsMsg::Err(format!("Couldn't send logs to {}: {e}", target.name)),
    };
    let url = format!(
        "{}/api/v1/client-logs",
        library::base_url(&target.addr, target.mgmt_port)
    );
    match agent
        .post(url.as_str())
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(_) => SendLogsMsg::Ok(format!("Logs sent to {} — thank you!", target.name)),
        Err(e) => match library::classify(e) {
            LibraryError::Http(413) => SendLogsMsg::Err("Log file too large to send (1 MB limit).".into()),
            LibraryError::NotPaired => {
                SendLogsMsg::Err(format!("{} refused the logs — pair with it again.", target.name))
            }
            other => SendLogsMsg::Err(format!(
                "{}: {}",
                target.name,
                crate::app::view::hostpower::refusal_message(&other)
            )),
        },
    }
}

/// Reads the log file and POSTs it as a multipart `file` field. Runs on the upload
/// worker thread — never on the UI thread. Maps the service's status codes (see the
/// Go handler: 429 rate-limited, 413 too large) to friendly status lines.
fn upload_logs(path: &Path) -> SendLogsMsg {
    const BOUNDARY: &str = "----punktfunkwebos7f3a2c1b8e4d6f0a";
    let data = match std::fs::read(path) {
        Ok(d) if !d.is_empty() => d,
        Ok(_) => return SendLogsMsg::Err("No logs to send yet.".into()),
        Err(e) => return SendLogsMsg::Err(format!("Couldn't read the log file: {e}")),
    };
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("punktfunk-webos.log");

    let mut body = Vec::with_capacity(data.len() + 256);
    body.extend_from_slice(
        format!(
            "--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n\
             Content-Type: text/plain\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(&data);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={BOUNDARY}");
    let agent = ureq::Agent::new_with_defaults();
    match agent
        .post(UPLOAD_URL)
        .header("Content-Type", &content_type)
        .send(&body[..])
    {
        Ok(_) => SendLogsMsg::Ok("Logs sent to the developer — thank you!".into()),
        Err(ureq::Error::StatusCode(429)) => {
            SendLogsMsg::Err("Rate limited — wait a minute before sending logs again.".into())
        }
        Err(ureq::Error::StatusCode(413)) => SendLogsMsg::Err("Log file too large to send (4 MB limit).".into()),
        Err(ureq::Error::StatusCode(code)) => SendLogsMsg::Err(format!("Upload failed (HTTP {code}).")),
        Err(e) => SendLogsMsg::Err(format!("Couldn't reach the log server: {e}")),
    }
}
