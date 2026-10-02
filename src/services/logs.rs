//! Sends the session log to a paired host from the console's host menu. Blocking; the caller
//! runs it on a worker.
use crate::services::library::{self, LibraryError};
use std::path::Path;

/// The tail of the log that travels; the file itself rotates at this size too.
const MAX_LOG_BYTES: u64 = 960 * 1024;

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
    let unreadable = |e: std::io::Error| format!("Couldn't read the log file: {e}");
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
    let path = crate::logger::latest_log_file(&dir).ok_or("No logs to send yet.")?;
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
            other => format!("{}: {other}", target.name),
        }),
    }
}
