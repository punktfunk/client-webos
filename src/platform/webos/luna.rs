//! Minimal one-shot Luna (webOS service bus) caller: `device`'s system queries, [`launch_home`],
//! and [`crate::platform::webos::dualsense`]'s feedback when the in-process bus can't register.
//!
//! **Why a subprocess as well as `libluna-service2`.** In-process `LSCall` needs a registered
//! `LSHandle` attached to a running `GMainLoop`, and LS2 authorizes a caller by *which
//! executable* is calling: permissions come from the role file matched to the binary's path
//! (`/usr/share/luna-service2/roles.d/`). Registering from the app's own binary is verified on a
//! G5 and is the fast path ([`crate::platform::webos::ls2`]); a hub that refuses it still leaves
//! `luna-send-pub`, whose own role was verified on-device to reach the Bluetooth HID methods
//! from a dev-mode install.
//!
//! Cost is one fork/exec per call, which is why nothing here is on a hot path: the feedback
//! sender coalesces to the latest state and sends from its own thread (see
//! [`crate::platform::webos::dualsense::Feedback`]). Never call this from the render/input loop.
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Public-bus variant deliberately: on webOS 5.x-10.x `/usr/bin/luna-send` is `0700 root`
/// and unusable from an app's uid, while `luna-send-pub` is `0755` (verified on webOS 10.3).
const LUNA_SEND_PUB: &str = "/usr/bin/luna-send-pub";

/// A call that never answers must not wedge the caller's thread — `getReport` on a
/// disconnected pad does exactly that (verified on-device), so every child gets a deadline
/// and a kill rather than an unbounded wait.
///
/// Kept short deliberately: this bounds how long session teardown can wait for the pad to be
/// handed back (see `crate::platform::webos::dualsense::Feedback::release`), and a send that hasn't answered in
/// this long means the Bluetooth service is wedged or the pad is gone — in which case there is
/// nothing left to hand back. A healthy send answers in tens of milliseconds.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_millis(800);

/// Whether the public-bus tool exists and is executable, probed once. A TV without it
/// (or an install whose jail hides it) simply gets no `DualSense` feedback — every caller
/// treats this as "feature absent", never as an error worth surfacing.
pub fn available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        let ok = std::fs::metadata(LUNA_SEND_PUB).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        });
        if !ok {
            tracing::info!("{LUNA_SEND_PUB} not executable here — DualSense feedback disabled");
        }
        ok
    })
}

/// Opens the webOS launcher by relaunching the Home app — the stand-in for the OS's Home
/// shortcut once `KEYS_HOME` capture takes it over (capture being what stops webOS killing
/// the app on Home). Fire-and-forget: a Home press must not block the input loop.
pub fn launch_home() {
    std::thread::spawn(|| {
        if let Err(e) = call(
            "luna://com.webos.applicationManager/launch",
            r#"{"id":"com.webos.app.home"}"#,
        ) {
            tracing::warn!("launch webOS home failed: {e:#}");
        }
    });
}

/// Fires one `luna://` call and discards the reply. Blocking (bounded by [`CALL_TIMEOUT`]);
/// `Err` covers spawn failure, the deadline, and a non-zero exit.
///
/// The reply is discarded rather than parsed because the useful failures are not in it:
/// a wrong payload shape answers `returnValue:false` with an error code, which is a bug to
/// fix during bring-up, not a runtime condition to branch on. Feedback is idempotent — the
/// next state update re-sends everything — so a dropped call needs no recovery path.
pub fn call(uri: &str, payload: &str) -> anyhow::Result<()> {
    run_bounded(&["-n", "1", uri, payload], CALL_TIMEOUT, false).map(|_| ())
}

/// Like [`call`] but waits for and returns the reply JSON on stdout — for the callers that
/// must read a value back (e.g. `getSystemSettings`, or an `exec` service that returns command
/// output). Non-interactive `luna-send-pub` only waits for the reply when given `-w` (without
/// it, it fires and exits before the reply arrives — verified on-device, webOS 10.3), so the
/// caller's `timeout` is passed through as `-w` AND, plus a margin, as the kill deadline.
pub fn call_capture(uri: &str, payload: &str, timeout: Duration) -> anyhow::Result<String> {
    let wait_ms = timeout.as_millis().to_string();
    // Own deadline sits past `-w` so `luna-send-pub` exits on its own timeout first.
    run_bounded(
        &["-n", "1", "-w", &wait_ms, uri, payload],
        timeout + Duration::from_millis(500),
        true,
    )
}

/// Spawns `luna-send-pub` with `args`, polling until it exits or `timeout` elapses (killing and
/// reaping on timeout so no zombie is left). Returns its stdout when `capture` is set, else an
/// empty string. Shared by [`call`] and [`call_capture`] — the only differences between them are
/// the args, `capture`, and the deadline.
///
/// **Why a temp file and not a pipe.** `luna-send-pub` leaves stdout fully buffered when it is
/// not a tty and exits without flushing, so a piped reply arrives empty every time (verified
/// on-device: the same call redirected to a file prints its JSON). A regular file is the one
/// non-tty destination that survives that, since the buffer is flushed by the kernel on exit.
fn run_bounded(args: &[&str], timeout: Duration, capture: bool) -> anyhow::Result<String> {
    if !available() {
        anyhow::bail!("luna-send-pub unavailable");
    }
    let reply = capture.then(ReplyFile::new);
    let stdout = match &reply {
        Some(reply) => Stdio::from(std::fs::File::create(&reply.0)?),
        None => Stdio::null(),
    };
    let mut child = Command::new(LUNA_SEND_PUB)
        .args(args)
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => {
                return match &reply {
                    Some(reply) => reply.read(),
                    None => Ok(String::new()),
                };
            }
            Some(status) => anyhow::bail!("luna-send-pub exited {status}"),
            None if Instant::now() >= deadline => {
                // Kill AND reap: an unreaped child becomes a zombie held for the life of
                // the app, and this path repeats for every send once a call starts hanging.
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("luna-send-pub timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

/// The file one call's reply is written to, unlinked on drop so every way out of
/// [`run_bounded`] — including ones added later — cleans up without having to remember to.
///
/// The path is unique per process AND per call: calls can be in flight from several threads
/// (feedback sends, the root probe), and a shared path would let one truncate another's reply.
struct ReplyFile(std::path::PathBuf);

impl ReplyFile {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!("pf-luna-{}-{n}.json", std::process::id())))
    }

    fn read(&self) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(&self.0)?)
    }
}

impl Drop for ReplyFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
