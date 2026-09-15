//! In-process Luna bus client (`libluna-service2`), the fast path behind [`crate::platform::webos::luna`].
//!
//! **Why this exists next to `luna-send-pub`.** The subprocess route costs a fork/exec of a
//! process holding SDL and the decoder per call, which is why `DualSense` feedback is throttled to
//! four sends a second. Audio to the pad needs ~94 reports a second, so it needs a function call,
//! not a spawn. Verified on a G5 (webOS 10.3, dev-mode): `LSRegister` under the app id succeeds
//! **from the app's own binary path** (the hub keys permissions on the executable — the same call
//! from a binary elsewhere answers "Invalid permissions"), and `bluetooth2` methods reply in 1–3 ms.
//!
//! Resolved with `dlopen` rather than linked: the preview container has no `libluna-service2`,
//! and a TV whose hub refuses the registration must degrade to the subprocess route, not fail.
//!
//! Thread affinity: an `LSHandle` is pumped by the `GMainContext` it is attached to, so a [`Bus`]
//! is created, used and dropped on one thread ([`Bus`] is `!Send` by construction). Calls are
//! asynchronous; [`Bus::pump`] dispatches the replies that have arrived.
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Result};

use super::dl;

/// Registered bus name, tried first. SAM registers the running app under its id, so inside the
/// launched app this is "already exists" and the handle is registered anonymously instead — the
/// hub grants the dev-mode `public` group by executable path either way (both verified on-device).
const BUS_NAME: &CStr = c"io.unom.punktfunk.client-webos";
const LS2_LIB: &CStr = c"libluna-service2.so.3";
const GLIB_LIB: &CStr = c"libglib-2.0.so.0";

/// `struct LSError` is six words on this ABI; oversized so a newer library's `LSErrorInit` can
/// never write past it. Only `error_code` and `message` are read.
#[repr(C)]
struct LsError {
    error_code: c_int,
    message: *mut c_char,
    _rest: [usize; 14],
}

type Handle = *mut c_void;
type Message = *mut c_void;
type Token = std::ffi::c_ulong;
type Filter = unsafe extern "C" fn(Handle, Message, *mut c_void) -> bool;

struct Fns {
    error_init: unsafe extern "C" fn(*mut LsError),
    error_free: unsafe extern "C" fn(*mut LsError),
    register: unsafe extern "C" fn(*const c_char, *mut Handle, *mut LsError) -> bool,
    unregister: unsafe extern "C" fn(Handle, *mut LsError) -> bool,
    context_attach: unsafe extern "C" fn(Handle, *mut c_void, *mut LsError) -> bool,
    call_one_reply: unsafe extern "C" fn(
        Handle,
        *const c_char,
        *const c_char,
        Filter,
        *mut c_void,
        *mut Token,
        *mut LsError,
    ) -> bool,
    message_payload: unsafe extern "C" fn(Message) -> *const c_char,
    context_new: unsafe extern "C" fn() -> *mut c_void,
    context_iteration: unsafe extern "C" fn(*mut c_void, c_int) -> c_int,
    context_unref: unsafe extern "C" fn(*mut c_void),
}

fn fns() -> Result<&'static Fns> {
    static FNS: OnceLock<std::result::Result<Fns, String>> = OnceLock::new();
    dl::cached(&FNS, LS2_LIB, |lib| {
        let glib = dl::Lib::open(GLIB_LIB)?;
        Ok(Fns {
            error_init: lib.sym(c"LSErrorInit")?,
            error_free: lib.sym(c"LSErrorFree")?,
            register: lib.sym(c"LSRegister")?,
            unregister: lib.sym(c"LSUnregister")?,
            context_attach: lib.sym(c"LSGmainContextAttach")?,
            call_one_reply: lib.sym(c"LSCallOneReply")?,
            message_payload: lib.sym(c"LSMessageGetPayload")?,
            context_new: glib.sym(c"g_main_context_new")?,
            context_iteration: glib.sym(c"g_main_context_iteration")?,
            context_unref: glib.sym(c"g_main_context_unref")?,
        })
    })
}

/// Which call a reply belongs to, so an asynchronous refusal can name itself. Rides the
/// callback's context pointer as a plain integer and is never dereferenced, so there is no
/// lifetime to manage — the hub may deliver the reply long after the caller moved on.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Call {
    SendReport = 1,
    StopSniff = 2,
    StartSniff = 3,
}

impl Call {
    fn name(raw: usize) -> &'static str {
        match raw {
            1 => "sendData",
            2 => "stopSniff",
            3 => "startSniff",
            _ => "unknown call",
        }
    }
}

/// Reply bookkeeping shared by every [`Bus`]: the counts the stats overlay and the logs read.
pub struct Replies {
    pub ok: AtomicU32,
    pub failed: AtomicU32,
    /// Latched by the first "device not available" refusal: see [`on_reply`].
    unavailable: AtomicBool,
    /// The first failing reply since the counters were last read, for one log line per run.
    first_failure: Mutex<Option<String>>,
}

pub static REPLIES: Replies = Replies {
    ok: AtomicU32::new(0),
    failed: AtomicU32::new(0),
    unavailable: AtomicBool::new(false),
    first_failure: Mutex::new(None),
};

impl Replies {
    /// Takes the first failure text recorded since the last take, if any.
    pub fn take_failure(&self) -> Option<String> {
        self.first_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

/// Every call's reply lands here. Runs on the pumping thread, inside [`Bus::pump`].
unsafe extern "C" fn on_reply(_sh: Handle, reply: Message, ctx: *mut c_void) -> bool {
    let Ok(f) = fns() else { return true };
    // SAFETY: `reply` is the live message the hub delivered for this callback.
    let payload = unsafe { (f.message_payload)(reply) };
    let text = if payload.is_null() {
        String::from("(no payload)")
    } else {
        // SAFETY: LS2 payloads are NUL-terminated JSON owned by the message for the callback.
        unsafe { CStr::from_ptr(payload) }.to_string_lossy().into_owned()
    };
    if text.contains("\"returnValue\":true") {
        REPLIES.ok.fetch_add(1, Ordering::Relaxed);
    } else {
        REPLIES.failed.fetch_add(1, Ordering::Relaxed);
        // A set with no HID write path for a lane answers every single report with `106`
        // ("Device with supplied address is not available"). One line explains the missing
        // feedback; 94 a second explains nothing, so latch it and drop the rest on the floor.
        if text.contains("\"errorCode\":106") && REPLIES.unavailable.swap(true, Ordering::Relaxed) {
            return true;
        }
        let mut first = REPLIES
            .first_failure
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        first.get_or_insert(format!("{} refused: {text}", Call::name(ctx as usize)));
    }
    true
}

/// One registered handle on a private main context. Not `Send`: see the module docs.
pub struct Bus {
    handle: Handle,
    context: *mut c_void,
    fns: &'static Fns,
}

impl Bus {
    /// Registers under the app id. Fails (softly, for callers to fall back) when the library is
    /// absent or the hub refuses this executable.
    pub fn open() -> Result<Self> {
        let fns = fns()?;
        let mut err = LsError {
            error_code: 0,
            message: std::ptr::null_mut(),
            _rest: [0; 14],
        };
        let mut handle: Handle = std::ptr::null_mut();
        // SAFETY: all pointers are to live locals; `BUS_NAME` is NUL-terminated.
        let mut ok = unsafe {
            (fns.error_init)(&mut err);
            (fns.register)(BUS_NAME.as_ptr(), &mut handle, &mut err)
        };
        if !ok || handle.is_null() {
            let named = describe(&err);
            // SAFETY: `err` was initialised by `LSErrorInit`; re-initialised before reuse.
            ok = unsafe {
                (fns.error_free)(&mut err);
                (fns.error_init)(&mut err);
                (fns.register)(std::ptr::null(), &mut handle, &mut err)
            };
            if !ok || handle.is_null() {
                let msg = describe(&err);
                // SAFETY: as above.
                unsafe { (fns.error_free)(&mut err) };
                bail!("LSRegister refused: named {named}; anonymous {msg}");
            }
            tracing::debug!("Luna bus: {BUS_NAME:?} taken ({named}); registered anonymously");
        }
        // SAFETY: a fresh context; the handle is attached before any call.
        let context = unsafe { (fns.context_new)() };
        // SAFETY: `handle` registered above, `context` live, `err` initialised.
        let attached = unsafe { (fns.context_attach)(handle, context, &mut err) };
        if !attached {
            let msg = describe(&err);
            // SAFETY: as above; the handle is released on the failure path.
            unsafe {
                (fns.error_free)(&mut err);
                (fns.unregister)(handle, &mut err);
                (fns.context_unref)(context);
            }
            bail!("LSGmainContextAttach failed: {msg}");
        }
        Ok(Self { handle, context, fns })
    }

    /// Fires one call; the reply is counted by [`on_reply`] when [`pump`](Self::pump) runs.
    /// `Err` is the hub refusing to accept the call at all, not a failing reply.
    pub fn call(&self, uri: &str, payload: &str, what: Call) -> Result<()> {
        let uri = CString::new(uri)?;
        let payload = CString::new(payload)?;
        let mut err = LsError {
            error_code: 0,
            message: std::ptr::null_mut(),
            _rest: [0; 14],
        };
        let mut token: Token = 0;
        // SAFETY: handle attached in `open`; strings NUL-terminated and outlive the call, which
        // copies them; the context is an integer the callback only ever reads back as one.
        let ok = unsafe {
            (self.fns.error_init)(&mut err);
            (self.fns.call_one_reply)(
                self.handle,
                uri.as_ptr(),
                payload.as_ptr(),
                on_reply,
                what as usize as *mut c_void,
                &mut token,
                &mut err,
            )
        };
        if !ok {
            let msg = describe(&err);
            // SAFETY: initialised by `LSErrorInit` above.
            unsafe { (self.fns.error_free)(&mut err) };
            bail!("LSCallOneReply failed: {msg}");
        }
        Ok(())
    }

    /// Dispatches every reply that has arrived; returns without blocking.
    pub fn pump(&self) {
        // SAFETY: `context` is live for the life of `self`; non-blocking iteration.
        while unsafe { (self.fns.context_iteration)(self.context, 0) } != 0 {}
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        // Give in-flight replies a moment so the last report's outcome is counted, then release.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
        while std::time::Instant::now() < deadline {
            self.pump();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut err = LsError {
            error_code: 0,
            message: std::ptr::null_mut(),
            _rest: [0; 14],
        };
        // SAFETY: handle and context were created in `open` and are released exactly once here.
        unsafe {
            (self.fns.error_init)(&mut err);
            (self.fns.unregister)(self.handle, &mut err);
            (self.fns.error_free)(&mut err);
            (self.fns.context_unref)(self.context);
        }
    }
}

fn describe(err: &LsError) -> String {
    if err.message.is_null() {
        format!("code {}", err.error_code)
    } else {
        // SAFETY: LS2 sets `message` to a NUL-terminated string it owns until `LSErrorFree`.
        let msg = unsafe { CStr::from_ptr(err.message) }.to_string_lossy();
        format!("{msg} (code {})", err.error_code)
    }
}
