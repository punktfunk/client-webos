//! The in-stream quick-action dial's pad side, in wire bits so it tests without SDL.
//!
//! A pad shortcut opens the dial ([`pad_shortcuts`]), and so does a held Yellow on the remote
//! ([`TapHold`]), for a stream with no pad on this TV. Opening releases whatever the host still
//! holds, and while the dial is open every press the shortcut engine forwards drives it
//! instead. Buttons still down when it closes stay the dial's until they release, so the B that
//! closed it never lands in the game.
//!
//! A, B and Y here are labels (`face`): the wire bits stay positional, the dial reads like the
//! menus do.

use std::time::{Duration, Instant};

use pf_client_core::menu_nav::{ring_sector, MenuDir, MenuEvent};
use pf_client_core::overlay_actions::{OverlayConfig, RingPlatform, SlotId};
use pf_client_core::shortcuts::{resolve, Action, Client, Shortcuts};
use punktfunk_core::input::gamepad::{
    BTN_A, BTN_B, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT, BTN_DPAD_UP, BTN_X, BTN_Y,
};

/// The pad shortcuts a stream runs: the stored `shortcuts` blob or this TV's defaults, less
/// the actions this client cannot take. A key combination needs its keys in the `ring` blob.
pub fn pad_shortcuts(blob: &str, ring: &str) -> Shortcuts {
    use SlotId as S;
    let ring = OverlayConfig::parse(ring, RingPlatform::Desktop);
    let mut shortcuts = resolve(blob, Client::WebOS, false, "");
    shortcuts.bindings.retain(|b| match &b.action {
        Action::Slot(S::Shortcut(id)) => ring.shortcut(id).is_some(),
        Action::Ring
        | Action::Slot(
            S::EndStream | S::DisconnectLinger | S::Stats | S::Keyboard | S::Guide | S::Qam | S::PadMouse | S::PadType,
        ) => true,
        _ => false,
    });
    shortcuts
}

/// What the stream loop does with one pad button edge the shortcut engine forwarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadRoute {
    /// Send it to the host as usual.
    Forward,
    /// The dial owns it; send nothing.
    Drop,
    /// The dial is open: feed it this.
    Menu(MenuEvent),
}

#[derive(Debug)]
pub struct PadDial {
    /// The wire bits under the A, B, X and Y labels.
    face: [u32; 4],
    /// Physically down.
    held: u32,
    /// Down on the host.
    sent: u32,
    /// Taken by the dial; their releases stay local.
    owned: u32,
    /// Raw SDL left stick, +y down.
    stick: (i16, i16),
    sector: Option<u8>,
}

impl Default for PadDial {
    fn default() -> Self {
        Self::with_face([BTN_A, BTN_B, BTN_X, BTN_Y])
    }
}

impl PadDial {
    /// A dial for a pad whose A, B, X and Y labels sit on these wire bits.
    pub fn with_face(face: [u32; 4]) -> Self {
        Self {
            face,
            held: 0,
            sent: 0,
            owned: 0,
            stick: (0, 0),
            sector: None,
        }
    }

    /// Routes one button edge. `open` is whether the dial is up right now.
    pub fn button(&mut self, bit: u32, down: bool, open: bool) -> PadRoute {
        if down {
            self.held |= bit;
        } else {
            self.held &= !bit;
            if self.owned & bit != 0 {
                self.owned &= !bit;
                return PadRoute::Drop;
            }
        }
        if open {
            if !down {
                return PadRoute::Drop;
            }
            self.owned |= bit;
            return self.menu_for(bit).map_or(PadRoute::Drop, PadRoute::Menu);
        }
        if down {
            self.sent |= bit;
        } else {
            self.sent &= !bit;
        }
        PadRoute::Forward
    }

    /// The dial just opened: the buttons the host still holds, to release there.
    pub fn opened(&mut self) -> u32 {
        self.sector = None;
        std::mem::take(&mut self.sent)
    }

    /// The dial just closed: whatever is still down belongs to it until released.
    pub fn closed(&mut self) {
        self.owned |= self.held;
        self.sector = None;
    }

    /// One left-stick axis (`x` or not), raw SDL. While open, the slot it now aims at.
    pub fn left_stick(&mut self, x: bool, value: i16, open: bool) -> Option<MenuEvent> {
        if x {
            self.stick.0 = value;
        } else {
            self.stick.1 = value;
        }
        if !open {
            return None;
        }
        let sector = ring_sector(self.stick.0, self.stick.1, self.sector);
        (sector != self.sector).then(|| {
            self.sector = sector;
            MenuEvent::Sector(sector)
        })
    }

    /// The pad went away: nothing it held can release any more.
    pub fn clear(&mut self) {
        *self = Self::with_face(self.face);
    }

    fn menu_for(&self, bit: u32) -> Option<MenuEvent> {
        let [a, b, _, y] = self.face;
        Some(match bit {
            _ if bit == a => MenuEvent::Confirm,
            _ if bit == b => MenuEvent::Back,
            _ if bit == y => MenuEvent::Secondary,
            BTN_DPAD_UP => MenuEvent::Move(MenuDir::Up),
            BTN_DPAD_DOWN => MenuEvent::Move(MenuDir::Down),
            BTN_DPAD_LEFT => MenuEvent::Move(MenuDir::Left),
            BTN_DPAD_RIGHT => MenuEvent::Move(MenuDir::Right),
            _ => return None,
        })
    }
}

/// How long Yellow is held to open the dial. A shorter press keeps the key's tap action.
const YELLOW_HOLD: Duration = Duration::from_millis(600);

/// What a [`TapHold`] key did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Press {
    /// Released before the hold threshold.
    Tap,
    /// Still down at the threshold.
    Hold,
}

/// One remote key that taps one action and holds another. The hold fires while the key is still
/// down, so its release is not also a tap; the tap waits for the release, since only then is it
/// known not to be a hold.
#[derive(Debug, Default)]
pub struct TapHold {
    /// When the key went down; `None` when up, or once the hold fired.
    since: Option<Instant>,
    tapped: bool,
}

impl TapHold {
    /// One edge of the key.
    pub fn edge(&mut self, down: bool, now: Instant) {
        if down {
            self.since = Some(now);
        } else if self.since.take().is_some() {
            self.tapped = true;
        }
    }

    /// Reports the press once: a tap after its release, a hold once [`YELLOW_HOLD`] has passed.
    pub fn poll(&mut self, now: Instant) -> Option<Press> {
        if std::mem::take(&mut self.tapped) {
            return Some(Press::Tap);
        }
        let since = self.since?;
        (now.duration_since(since) >= YELLOW_HOLD).then(|| {
            self.since = None;
            Press::Hold
        })
    }

    /// Forgets the key, for when its release will not arrive.
    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pf_client_core::shortcuts::{Device, Engine, Event, Out, Phase, HOLD};
    use punktfunk_core::input::gamepad::{BTN_BACK, BTN_GUIDE, BTN_LB, BTN_RB, BTN_START};

    #[test]
    fn a_short_press_taps_on_release() {
        let t = Instant::now();
        let mut k = TapHold::default();
        k.edge(true, t);
        assert_eq!(k.poll(t + YELLOW_HOLD / 2), None);
        k.edge(false, t + YELLOW_HOLD / 2);
        assert_eq!(k.poll(t + YELLOW_HOLD), Some(Press::Tap));
        assert_eq!(k.poll(t + YELLOW_HOLD * 2), None, "once");
    }

    #[test]
    fn a_hold_fires_while_down_and_its_release_is_no_tap() {
        let t = Instant::now();
        let mut k = TapHold::default();
        k.edge(true, t);
        assert_eq!(k.poll(t + YELLOW_HOLD), Some(Press::Hold));
        assert_eq!(k.poll(t + YELLOW_HOLD * 2), None, "once");
        k.edge(false, t + YELLOW_HOLD * 3);
        assert_eq!(k.poll(t + YELLOW_HOLD * 3), None);
    }

    #[test]
    fn a_cleared_key_neither_holds_nor_taps() {
        let t = Instant::now();
        let mut k = TapHold::default();
        k.edge(true, t);
        k.clear();
        assert_eq!(k.poll(t + YELLOW_HOLD), None);
        k.edge(false, t + YELLOW_HOLD);
        assert_eq!(k.poll(t + YELLOW_HOLD), None);
    }

    fn engine() -> Engine {
        Engine::new(&pad_shortcuts("", "").bindings, Device::Pad)
    }

    #[test]
    fn select_then_a_opens_the_dial_and_a_never_reaches_the_host() {
        let (mut e, t) = (engine(), Instant::now());
        assert_eq!(e.step(Event::Press(BTN_BACK), t), [Out::Forward(BTN_BACK, true)]);
        assert_eq!(
            e.step(Event::Press(BTN_A), t),
            [Out::Lift(vec![BTN_BACK]), Out::Fire(Action::Ring, Phase::Once)]
        );
        assert!(e.step(Event::Release(BTN_A), t).is_empty());
        assert!(e.step(Event::Release(BTN_BACK), t).is_empty());
    }

    #[test]
    fn a_alone_is_game_input() {
        let (mut e, t) = (engine(), Instant::now());
        assert_eq!(e.step(Event::Press(BTN_A), t), [Out::Forward(BTN_A, true)]);
        assert_eq!(e.step(Event::Release(BTN_A), t), [Out::Forward(BTN_A, false)]);
    }

    #[test]
    fn only_the_full_chord_held_a_second_ends_the_stream() {
        let t = Instant::now();
        let mut e = engine();
        for b in [BTN_LB, BTN_RB, BTN_START, BTN_BACK] {
            e.step(Event::Press(b), t);
        }
        assert!(e.tick(t + HOLD / 2).is_empty());
        assert_eq!(
            e.tick(t + HOLD),
            [
                Out::Lift(vec![BTN_LB, BTN_RB, BTN_START]),
                Out::Fire(Action::Slot(SlotId::EndStream), Phase::Once)
            ]
        );
        assert!(e.tick(t + HOLD * 2).is_empty(), "once per hold");
        for partial in [
            &[BTN_GUIDE][..],
            &[BTN_LB, BTN_RB],
            &[BTN_START, BTN_BACK],
            &[BTN_GUIDE, BTN_LB, BTN_RB, BTN_START],
        ] {
            let mut e = engine();
            for &b in partial {
                e.step(Event::Press(b), t);
            }
            assert!(e.tick(t + HOLD).is_empty(), "{partial:?} fired");
        }
    }

    #[test]
    fn actions_this_tv_cannot_take_are_dropped() {
        let blob = r#"{"v":1,"bindings":[
            {"on":"pad","keys":["lb","rb"],"do":"fullscreen"},
            {"on":"pad","keys":["lb","x"],"do":"stats"},
            {"on":"pad","keys":["lb","y"],"do":"mystery"}]}"#;
        let kept: Vec<Action> = pad_shortcuts(blob, "").bindings.into_iter().map(|b| b.action).collect();
        assert_eq!(kept, [Action::Slot(SlotId::Stats)]);
    }

    #[test]
    fn presses_drive_the_open_dial() {
        let mut d = PadDial::default();
        assert_eq!(
            d.button(BTN_DPAD_RIGHT, true, true),
            PadRoute::Menu(MenuEvent::Move(MenuDir::Right))
        );
        assert_eq!(d.button(BTN_B, true, true), PadRoute::Menu(MenuEvent::Back));
        assert_eq!(
            d.button(gamepad_start(), true, true),
            PadRoute::Drop,
            "unmapped buttons stay local"
        );
    }

    #[test]
    fn the_button_that_closed_it_never_reaches_the_game() {
        let mut d = PadDial::default();
        assert_eq!(d.button(BTN_B, true, true), PadRoute::Menu(MenuEvent::Back));
        d.closed();
        assert_eq!(d.button(BTN_B, false, false), PadRoute::Drop);
        assert_eq!(
            d.button(BTN_B, true, false),
            PadRoute::Forward,
            "a fresh press is the game's"
        );
    }

    #[test]
    fn the_stick_aims_only_while_open() {
        let mut d = PadDial::default();
        assert_eq!(d.left_stick(false, -32000, false), None);
        assert_eq!(
            d.left_stick(true, 0, true),
            Some(MenuEvent::Sector(Some(0))),
            "up is 12 o'clock"
        );
        assert_eq!(d.left_stick(true, 0, true), None, "no change, no event");
        assert_eq!(d.left_stick(false, 0, true), Some(MenuEvent::Sector(None)));
    }

    fn gamepad_start() -> u32 {
        punktfunk_core::input::gamepad::BTN_START
    }
}
