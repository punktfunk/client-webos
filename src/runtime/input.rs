//! Input plumbing the loops share: the disconnect chord, the remote gate, the on-screen
//! keyboard, and the webOS EXIT/Home keys. Re-exported from `runtime` via `use input::*`.

use super::*;

/// How long the controller chord ([`DisconnectChord`]) must be held before the in-stream
/// disconnect dialog opens. The chord's buttons are also game input, so it fires on a hold,
/// not a press — 1s, to match webOS's own long-press threshold on the EXIT gesture.
pub(super) const EXIT_HOLD: Duration = Duration::from_millis(1000);

/// The gamepad route to the disconnect dialog:
/// L1+R1+Start+Select held for [`EXIT_HOLD`], the escape chord every punktfunk client uses.
/// No subset opens it — L1+R1 and a held Guide are game input.
///
/// Tracked as button state rather than read back from SDL because SDL only reports
/// transitions here — and a chord needs to know what is down *now*, not what changed
/// last.
#[derive(Default)]
pub(super) struct DisconnectChord {
    left_shoulder: bool,
    right_shoulder: bool,
    start: bool,
    back: bool,
    /// When the currently-held chord became complete; `None` when none is.
    since: Option<Instant>,
}

impl DisconnectChord {
    /// Records one button transition and arms or disarms the hold timer.
    pub(super) fn set(&mut self, button: sdl3::gamepad::Button, down: bool) {
        use sdl3::gamepad::Button;
        match button {
            Button::LeftShoulder => self.left_shoulder = down,
            Button::RightShoulder => self.right_shoulder = down,
            Button::Start => self.start = down,
            Button::Back => self.back = down,
            _ => return,
        }
        // Re-derived after every transition, so releasing any part of a chord restarts
        // the hold instead of leaving a stale deadline armed.
        self.since = match (self.complete(), self.since) {
            (true, Some(t)) => Some(t),
            (true, None) => Some(Instant::now()),
            (false, _) => None,
        };
    }

    fn complete(&self) -> bool {
        self.left_shoulder && self.right_shoulder && self.start && self.back
    }

    /// Whether a chord has now been held long enough to fire.
    pub(super) fn held_for(&self, hold: Duration) -> bool {
        self.since.is_some_and(|t| t.elapsed() >= hold)
    }

    /// Forgets all held buttons.
    ///
    /// Called when the chord fires and when the pad disconnects, because in both cases
    /// the releases that follow never reach [`set`](Self::set) — the open dialog swallows
    /// controller events, and an unplugged pad sends none. Without this the buttons would
    /// stay "down" forever and the dialog would reopen the instant it was dismissed.
    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }
}

/// Which of the compositor's keys the Magic Remote pressed. webOS 23+ also types every pad
/// press as a remote key, and a Wayland key event names no device, so SDL alone cannot tell
/// them apart. The remote's own evdev node can (`evdev::RemoteNode`): each press there admits
/// one compositor key-down of that key, and the key's repeats and release follow it.
///
/// The nodes are read on this thread, every tick and again before a key is turned away, so a
/// press behind a key SDL delivers is never still unread. The compositor still decides what a
/// press means (a pointer click, the EXIT gesture, a key for the on-screen keyboard); this only
/// says whose press it was.
///
/// Until a node is adopted the gate admits every key: a TV whose remote node the app cannot
/// open, or names differently, keeps its remote. A node that goes away afterwards does not
/// disarm it, since a key with no remote to press it is an echo; the reader opens it again
/// when it comes back.
#[derive(Default)]
pub(super) struct RemoteGate {
    /// The remote's own nodes.
    nodes: Vec<crate::platform::webos::evdev::RemoteNode>,
    /// A node was adopted at some point, so a key without a press behind it is an echo.
    armed: bool,
    /// Remote presses no compositor key-down has claimed yet, with the tick they were read in.
    owed: Vec<(u16, Instant)>,
    /// Keys admitted down, whose repeats and release pass.
    down: Vec<u16>,
}

impl RemoteGate {
    /// How long a remote press waits for its key-down. An OK the pointer turned into a click is
    /// never claimed and expires.
    const CLAIM_WINDOW: Duration = Duration::from_millis(250);

    /// Takes over remote nodes the evdev reader opened. The first one arms the gate.
    pub(super) fn adopt(&mut self, nodes: Vec<crate::platform::webos::evdev::RemoteNode>) {
        if !self.armed && !nodes.is_empty() {
            tracing::info!("remote gate armed: a key now needs a press on the remote's own node");
            self.armed = true;
        }
        self.nodes.extend(nodes);
    }

    /// Reads the remote's nodes; a node that is gone is dropped.
    pub(super) fn poll(&mut self, now: Instant) {
        self.owed.retain(|&(_, at)| now.duration_since(at) < Self::CLAIM_WINDOW);
        let owed = &mut self.owed;
        self.nodes.retain_mut(|node| node.drain(|code| owed.push((code, now))));
    }

    /// A gate with a remote node behind it, as a test stands in for one.
    #[cfg(test)]
    fn armed() -> Self {
        Self {
            armed: true,
            ..Self::default()
        }
    }

    /// A press off the remote's node, as a test stands in for one.
    #[cfg(test)]
    fn pressed(&mut self, code: u16, now: Instant) {
        self.owed.push((code, now));
    }

    /// Whether `event` may act: anything but a key does, and a key only if the remote pressed
    /// it. A pad's echo has no press behind it, so it, its repeats and its release all fail.
    pub(super) fn admits(&mut self, event: &sdl3::event::Event, now: Instant) -> bool {
        use sdl3::event::Event;
        if !self.armed {
            return true;
        }
        // `raw` is the Wayland key, i.e. the same evdev code the remote's node reports. A key the
        // remote lacks never has a press owed to it, so it fails below like an echo.
        let (code, down, repeat) = match *event {
            Event::KeyDown { raw, repeat, .. } => (raw, true, repeat),
            Event::KeyUp { raw, .. } => (raw, false, false),
            _ => return true,
        };
        self.owed.retain(|&(_, at)| now.duration_since(at) < Self::CLAIM_WINDOW);
        let held = self.down.iter().position(|&c| c == code);
        match (down, held) {
            (false, Some(i)) => {
                self.down.swap_remove(i);
                true
            }
            (true, Some(_)) => true,
            (false, None) => false,
            (true, None) if repeat => false,
            (true, None) => {
                if !self.owed.iter().any(|&(c, _)| c == code) {
                    // The press may have landed after this tick's poll: read before turning it away.
                    self.poll(now);
                }
                match self.owed.iter().position(|&(c, _)| c == code) {
                    Some(i) => {
                        self.owed.remove(i);
                        self.down.push(code);
                        true
                    }
                    None => false,
                }
            }
        }
    }
}

/// Rising-edge detect on a raw webOS scancode (polled since these sit outside
/// rust-sdl3's `Scancode` enum), firing once as it goes down. `prev` carries the last-frame
/// state across calls.
fn scancode_rising_edge(scancode: i32, prev: &mut bool) -> bool {
    let down = crate::platform::webos::input::webos_scancode_down(scancode);
    let fired = down && !*prev;
    *prev = down;
    fired
}

/// Controls the webOS on-screen keyboard for the stream's host-side text fields.
pub(super) struct TextInputController {
    util: sdl3::keyboard::TextInputUtil,
    /// A panel this app raised, and whether it has come up yet — see [`release_if_dismissed`].
    ///
    /// [`release_if_dismissed`]: Self::release_if_dismissed
    raised: Option<(std::time::Instant, bool)>,
}

/// Where the IME should put the caret inside the field rect. Nothing here drives a caret, so the
/// panel is told the field's start.
const IME_CURSOR: i32 = 0;

impl TextInputController {
    pub(super) fn new(util: sdl3::keyboard::TextInputUtil) -> Self {
        Self { util, raised: None }
    }

    /// Whether the panel is actually up. The compositor can dismiss it on its own (Back does
    /// exactly that) while text input stays on.
    pub(super) fn is_shown(&self, window: &sdl3::video::Window) -> bool {
        self.util.is_screen_keyboard_shown(window)
    }

    /// Raises the panel for a host-side field, whatever SDL thinks the current state is:
    /// re-starting an already-active input is how SDL3 re-shows a panel the compositor
    /// dismissed underneath it.
    pub(super) fn raise(&mut self, rect: sdl3::rect::Rect, window: &sdl3::video::Window) {
        self.util.set_rect(window, rect, IME_CURSOR);
        self.util.start(window);
        self.raised = Some((std::time::Instant::now(), false));
    }

    /// Ends the text input behind a panel webOS has taken away. Call every tick with
    /// [`is_shown`](Self::is_shown)'s answer.
    ///
    /// Back dismisses the panel through webOS's own IME without telling SDL, which leaves the
    /// field focused — and webOS routes every remote key to a focused field, so the remote reads
    /// as dead and a later OK re-summons the panel. Nothing else ends it.
    ///
    /// Waits for the panel to have actually appeared: `shown` is false for the ticks it takes to
    /// animate in. [`SHOW_TIMEOUT`](Self::SHOW_TIMEOUT) covers a raise the compositor refuses
    /// outright, which would otherwise hold the field for the whole session.
    pub(super) fn release_if_dismissed(&mut self, shown: bool, window: &sdl3::video::Window) {
        let Some((raised_at, came_up)) = &mut self.raised else {
            return;
        };
        if shown {
            *came_up = true;
            return;
        }
        if !*came_up && raised_at.elapsed() < Self::SHOW_TIMEOUT {
            return;
        }
        tracing::info!("on-screen keyboard dismissed — releasing text input");
        self.stop(window);
    }

    /// How long a raised panel has to appear before [`release_if_dismissed`] gives up on it.
    ///
    /// [`release_if_dismissed`]: Self::release_if_dismissed
    const SHOW_TIMEOUT: Duration = Duration::from_secs(2);

    /// Stops text input at loop exit.
    pub(super) fn stop(&mut self, window: &sdl3::video::Window) {
        self.raised = None;
        self.util.stop(window);
    }
}

/// The webOS EXIT gesture (a held Back, delivered as `WEBOS_EXIT_SCANCODE`).
pub(super) fn exit_gesture_fired(prev: &mut bool) -> bool {
    scancode_rising_edge(crate::platform::webos::input::WEBOS_EXIT_SCANCODE, prev)
}

/// The webOS Home key (`WEBOS_HOME_SCANCODE`) — captured, so callers re-open the
/// launcher themselves via `luna::launch_home`. Distinct from EXIT, so a long Back
/// never trips it.
pub(super) fn home_key_fired(prev: &mut bool) -> bool {
    scancode_rising_edge(crate::platform::webos::input::WEBOS_HOME_SCANCODE, prev)
}

#[cfg(test)]
mod remote_gate_tests {
    use super::*;
    use sdl3::event::Event;
    use sdl3::keyboard::{Keycode, Scancode};

    // `raw` is the evdev code, as the fork's Wayland backend reports it.
    use crate::platform::webos::input::{test_key_event as key, RemoteKey};

    fn up_key(down: bool, repeat: bool) -> Event {
        key(Some(Scancode::Up), Some(Keycode::Up), 103, down, repeat)
    }

    /// The remote's Back as SDL3 actually delivers it: nothing but `raw`.
    fn back_key(down: bool) -> Event {
        key(None, None, RemoteKey::Back as u16, down, false)
    }

    #[test]
    fn a_remote_press_admits_its_key_its_repeats_and_its_release() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        gate.pressed(103, t);
        let later = t + Duration::from_millis(20);
        assert!(gate.admits(&up_key(true, false), later));
        assert!(gate.admits(&up_key(true, true), later));
        assert!(gate.admits(&up_key(false, false), later));
        assert!(
            !gate.admits(&up_key(true, false), later),
            "one press admits one key-down"
        );
    }

    #[test]
    fn a_pad_echo_has_no_remote_press_behind_it() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        assert!(!gate.admits(&up_key(true, false), t));
        assert!(!gate.admits(&up_key(true, true), t));
        assert!(!gate.admits(&up_key(false, false), t));
        // The remote's Back: no scancode, no keycode, identified only by `raw`. Gated like any other key - the echo is refused, the real press admitted.
        let back = || back_key(true);
        assert!(!gate.admits(&back(), t));
        gate.pressed(RemoteKey::Back as u16, t);
        assert!(gate.admits(&back(), t));
    }

    #[test]
    fn rejected_back_echo_does_not_swallow_the_real_press() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        let mut keys = crate::platform::webos::input::RemoteKeys::default();
        let down = back_key(true);
        assert_eq!(keys.edge(&down, gate.admits(&down, t)), None);
        gate.pressed(RemoteKey::Back as u16, t);
        assert_eq!(keys.edge(&down, gate.admits(&down, t)), Some((RemoteKey::Back, true)));
        let up = back_key(false);
        assert_eq!(keys.edge(&up, gate.admits(&up, t)), Some((RemoteKey::Back, false)));
        assert_eq!(keys.edge(&up, gate.admits(&up, t)), None);
    }

    #[test]
    fn an_unclaimed_press_expires() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        // An OK the pointer turned into a click: no key-down ever claims it.
        gate.pressed(28, t);
        let enter = key(Some(Scancode::Return), Some(Keycode::Return), 28, true, false);
        assert!(!gate.admits(&enter, t + Duration::from_millis(300)));
    }

    #[test]
    fn keys_the_remote_lacks_never_pass_and_other_events_always_do() {
        let (mut gate, t) = (RemoteGate::armed(), Instant::now());
        let esc = key(Some(Scancode::Escape), Some(Keycode::Escape), 1, true, false);
        assert!(!gate.admits(&esc, t));
        assert!(gate.admits(&Event::Quit { timestamp: 0 }, t));
    }

    /// No remote node yet, or none this app can open: the old behaviour, every key passes.
    #[test]
    fn a_gate_without_a_remote_node_admits_every_key() {
        let (mut gate, t) = (RemoteGate::default(), Instant::now());
        assert!(gate.admits(&up_key(true, false), t));
        assert!(gate.admits(&up_key(false, false), t));
        let esc = key(Some(Scancode::Escape), Some(Keycode::Escape), 1, true, false);
        assert!(gate.admits(&esc, t));
    }
}

#[cfg(test)]
mod disconnect_chord_tests {
    use super::*;
    use sdl3::gamepad::Button::{self, Back, Guide, LeftShoulder, RightShoulder, Start};

    fn held(buttons: &[Button]) -> DisconnectChord {
        let mut chord = DisconnectChord::default();
        for &b in buttons {
            chord.set(b, true);
        }
        chord
    }

    #[test]
    fn only_the_full_chord_arms_the_dialog() {
        let mut chord = held(&[LeftShoulder, RightShoulder, Start, Back]);
        assert!(chord.held_for(Duration::ZERO));
        chord.set(Start, false);
        assert!(!chord.held_for(Duration::ZERO), "a released button kept the hold armed");
        for partial in [
            &[Guide][..],
            &[LeftShoulder, RightShoulder],
            &[Start, Back],
            &[Guide, LeftShoulder, RightShoulder, Start],
        ] {
            assert!(!held(partial).held_for(Duration::ZERO), "{partial:?} armed the dialog");
        }
    }
}

#[cfg(test)]
mod sdl_tests {
    use super::*;

    #[test]
    fn text_and_audio_lifetimes() {
        use crate::core::media::{AudioSink, Samples};
        use crate::platform::webos::audio::AudioPlayer;
        use std::sync::atomic::AtomicU32;
        use std::sync::Arc;

        // SDL's main-thread state and driver hints are process-global. Isolate from other tests.
        const CHILD: &str = "PUNKTFUNK_SDL_LIFETIME_TEST";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "runtime::input::sdl_tests::text_and_audio_lifetimes",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("SDL_AUDIO_DRIVER", "dummy")
                .env("SDL_VIDEO_DRIVER", "dummy")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let sdl = sdl3::init().unwrap();
        let video = sdl.video().unwrap();
        let window = video.window("input test", 1920, 1080).hidden().build().unwrap();
        let util = video.text_input();
        let mut input = TextInputController::new(video.text_input());
        let field = sdl3::rect::Rect::new(100, 700, 400, 60);
        input.raise(field, &window);
        assert!(util.is_active(&window));
        assert_eq!(util.rect(&window).unwrap(), (field, IME_CURSOR));
        input.stop(&window);
        assert!(!util.is_active(&window));

        let audio = sdl.audio().unwrap();
        for channels in [2, 6, 2, 6] {
            let depth = Arc::new(AtomicU32::new(0));
            let (player, sink) = AudioPlayer::new(&audio, channels, depth.clone()).unwrap();
            let pcm = vec![0.1; 240 * usize::from(channels)];
            for _ in 0..16 {
                sink.feed(Samples::F32(&pcm), 0).unwrap();
            }
            let deadline = Instant::now() + Duration::from_secs(2);
            while depth.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            assert!(depth.load(Ordering::Relaxed) > 0, "SDL callback never consumed PCM");
            // Tear down while playback is active, then reopen on the same subsystem.
            drop(player);
        }
    }
}
