//! Sends the session log to a paired host from the console's host menu. Blocking; the caller
//! runs it on a worker.
use crate::logger::MAX_LOG_BYTES;
use crate::services::library::{self, LibraryError};
use std::path::Path;

/// Host endpoint and credentials resolved before starting the worker.
/// Deliberately omits `Debug` because `identity` contains private key material.
pub struct HostTarget {
    pub name: String,
    pub addr: String,
    pub mgmt_port: u16,
    pub identity: (String, String),
    pub pin: [u8; 32],
}

/// Reads the newest `budget` bytes, dropping any partial leading line.
fn log_tail(path: &Path, budget: u64) -> Result<String, String> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let unreadable = |e: std::io::Error| pf_l10n::console::console_logs_read_failed(e);
    let mut f = std::fs::File::open(path).map_err(unreadable)?;
    let len = f.metadata().map_err(unreadable)?.len();
    let truncated = len > budget;
    if truncated {
        f.seek(SeekFrom::End(-(budget as i64))).map_err(unreadable)?;
    }
    let mut raw = Vec::with_capacity(len.min(budget) as usize);
    f.read_to_end(&mut raw).map_err(unreadable)?;
    // The seek may land within a UTF-8 character, so conversion remains lossy.
    let from = if truncated {
        raw.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)
    } else {
        0
    };
    let text = String::from_utf8_lossy(&raw[from..]);
    if text.trim().is_empty() {
        return Err(pf_l10n::console::console_logs_none().into());
    }
    Ok(if truncated {
        format!("… older log lines truncated …\n{text}")
    } else {
        text.into_owned()
    })
}

/// Posts the newest log tail to `target` as plain text on the paired mTLS identity. Blocks, so
/// call it from a worker. Either side of the result is the status line to show.
pub fn upload_to_host(target: &HostTarget) -> Result<String, String> {
    post_log(target)
        .inspect(|s| tracing::info!("send logs: {s}"))
        .inspect_err(|e| tracing::warn!("send logs failed: {e}"))
}

fn post_log(target: &HostTarget) -> Result<String, String> {
    let dir = crate::services::store::app_dir();
    // The run that crashed is the one before this one — a tester relaunches before reporting,
    // which rotates it out of the active log. Both travel, so neither case needs the other file.
    let previous = crate::logger::previous_log_file(&dir);
    let budget = if previous.is_some() {
        MAX_LOG_BYTES / 2
    } else {
        MAX_LOG_BYTES
    };
    let path = crate::logger::latest_log_file(&dir).ok_or(pf_l10n::console::console_logs_none())?;
    let log = log_tail(&path, budget)?;
    let previous = previous
        .filter(|p| *p != path)
        .and_then(|p| log_tail(&p, budget).ok())
        .map(|tail| format!("--- previous run ---\n{tail}\n--- this run ---\n"))
        .unwrap_or_default();
    let body = format!(
        "punktfunk-webos {} (webos {}) — client log bundle\n{previous}{log}",
        crate::core::VERSION,
        std::env::consts::ARCH,
    );
    let agent = library::agent(&target.identity, Some(target.pin))
        .map_err(|e| pf_l10n::console::console_logs_send_failed(&target.name, e))?;
    let url = format!(
        "{}/api/v1/client-logs",
        library::base_url(&target.addr, target.mgmt_port)
    );
    match agent
        .post(url.as_str())
        .header("Content-Type", "text/plain; charset=utf-8")
        .send(body.as_bytes())
    {
        Ok(_) => Ok(pf_l10n::client::hostcard_logs_sent_body(&target.name)),
        Err(e) => Err(match library::classify(e) {
            LibraryError::Http(413) => pf_l10n::console::console_logs_too_large().into(),
            LibraryError::NotPaired => pf_l10n::console::console_logs_refused(&target.name),
            other => pf_l10n::console::console_logs_failed_why(&target.name, other),
        }),
    }
}
