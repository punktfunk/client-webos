//! The in-stream quick-action dial's pad side, in wire bits so it tests without SDL.
//!
//! Select+A opens the dial: A never reaches the host, and neither does its release. Opening
//! releases whatever the host still holds, and while the dial is open every press drives it
//! instead. Buttons still down when it closes stay the dial's until they release, so the B that
//! closed it never lands in the game.
//!
//! A, B and Y here are labels (`face`): the wire bits stay positional, the dial reads like the
//! menus do.

use pf_client_core::menu_nav::{ring_sector, MenuDir, MenuEvent};
use punktfunk_core::input::gamepad::{
    BTN_A, BTN_B, BTN_BACK, BTN_DPAD_DOWN, BTN_DPAD_LEFT, BTN_DPAD_RIGHT, BTN_DPAD_UP, BTN_X, BTN_Y,
};

/// What the stream loop does with one pad button edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PadRoute {
    /// Send it to the host as usual.
    Forward,
    /// The dial owns it; send nothing.
    Drop,
    /// Select+A: open the dial.
    Open,
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
        if down && bit == self.face[0] && self.held & BTN_BACK != 0 {
            self.owned |= bit;
            return PadRoute::Open;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_then_a_opens_and_keeps_both_a_edges() {
        let mut d = PadDial::default();
        assert_eq!(d.button(BTN_BACK, true, false), PadRoute::Forward);
        assert_eq!(d.button(BTN_A, true, false), PadRoute::Open);
        assert_eq!(d.opened(), BTN_BACK, "the host still holds Select");
        assert_eq!(d.button(BTN_A, false, true), PadRoute::Drop);
        assert_eq!(d.button(BTN_BACK, false, true), PadRoute::Drop);
    }

    #[test]
    fn a_alone_or_a_first_is_game_input() {
        let mut d = PadDial::default();
        assert_eq!(d.button(BTN_A, true, false), PadRoute::Forward);
        assert_eq!(d.button(BTN_BACK, true, false), PadRoute::Forward);
        assert_eq!(d.button(BTN_A, false, false), PadRoute::Forward);
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
