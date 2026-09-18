//! Every attached game pad, each on its own wire pad index — the SDL3 port of
//! `pf_client_core::gamepad`'s slot table (the Linux client's).
//!
//! Every non-remote controller is opened, in the menus as in the stream: a second pad left shut
//! is dead for navigation too. Indices are lowest-free and stable for a slot's life, so an unplug
//! frees only its own index and a game never sees players shuffle.

use sdl3::gamepad::Gamepad;
use sdl3::joystick::JoystickId;

use super::input::DisconnectChord;
use crate::core::dial::PadDial;
use crate::platform::webos::gamepad;
use crate::services::store::GamepadType;

pub(super) struct Slot {
    /// SDL instance id — SDL3 names pads by ID only, no device indices.
    pub(super) id: JoystickId,
    /// Wire pad index (`InputEvent::flags`).
    pub(super) index: u8,
    pub(super) pad: Gamepad,
    /// Impulse-trigger motors. Read once: SDL walks its joystick list to answer.
    pub(super) triggers: bool,
    /// The kind this pad is; `None` uses Xbox default per `gamepad::kind_of`.
    pub(super) physical: Option<GamepadType>,
    /// The kind the host was last told this slot is; `None` before it has heard of it.
    pub(super) declared: Option<GamepadType>,
    /// Per pad: two pads' halves of a chord are not one person holding it.
    pub(super) chord: DisconnectChord,
    pub(super) dial: PadDial,
    /// `SDL_GetGamepadPath` and serial: what ties this pad to its own `DualSense` link.
    pub(super) path: Option<String>,
    pub(super) serial: Option<String>,
    /// The controller's `Uniq` (its MAC, for a `PlayStation` pad): what its touchpad and motion
    /// nodes carry, so their rich input reaches this slot.
    pub(super) uniq: Option<String>,
    /// This pad's `DualSense` lanes while a session runs.
    pub(super) extras: super::pad_session::Extras,
}

impl Slot {
    /// The kind the host should build this pad as. An explicit setting emulates that pad on every
    /// slot; `Automatic` mirrors the pad, and an unrecognized one takes the host's own default so
    /// a pad joining next to a `DualSense` is not built as another one.
    pub(super) fn host_kind(&self, setting: GamepadType) -> GamepadType {
        match setting {
            GamepadType::Auto => self.physical.unwrap_or(GamepadType::XboxOne),
            explicit => explicit,
        }
    }

    /// Whether this is a real `DualSense` the host builds as one — the pads with extras to route.
    pub(super) fn is_dualsense(&self, setting: GamepadType) -> bool {
        self.host_kind(setting).is_dualsense() && self.physical.is_some_and(GamepadType::is_dualsense)
    }
}

#[derive(Default)]
pub(super) struct Pads {
    /// In wire-index order, so the first is player 1.
    slots: Vec<Slot>,
}

/// Lowest free wire index, or `None` when every slot is taken.
fn lowest_free_index(taken: &[u8]) -> Option<u8> {
    (0..punktfunk_core::input::MAX_PADS as u8).find(|i| !taken.contains(i))
}

impl Pads {
    /// Opens pad `id`. `None` if remote, duplicate, table full, or open fails.
    pub(super) fn add(&mut self, subsystem: &sdl3::GamepadSubsystem, id: JoystickId) -> Option<&Slot> {
        if gamepad::is_remote_at(subsystem, id) {
            return None;
        }
        // Both `sync` and a queued `Added` event can name the same pad instance.
        // Check before open: SDL3 IDs the pad upfront, nothing new after opening.
        if self.slots.iter().any(|s| s.id == id) {
            return None;
        }
        let taken: Vec<u8> = self.slots.iter().map(|s| s.index).collect();
        let Some(index) = lowest_free_index(&taken) else {
            tracing::warn!("gamepad slots full — controller not forwarded");
            return None;
        };
        let pad = match subsystem.open(id) {
            Ok(pad) => pad,
            Err(e) => {
                tracing::warn!("controller open failed: {e}");
                return None;
            }
        };
        let name = pad.name().unwrap_or_default();
        let physical = gamepad::kind_of(&pad);
        let path = pad.path();
        let serial = pad.serial_number();
        let uniq = device_uniq(path.as_deref(), serial.as_deref());
        // SAFETY: SDL documents this as walking its joystick list for `pad`, which is open here.
        let triggers = unsafe { pad.has_rumble_triggers() };
        tracing::info!("controller connected: {name} (pad {index}, {physical:?})");
        let at = self.slots.partition_point(|s| s.index < index);
        self.slots.insert(
            at,
            Slot {
                id,
                index,
                triggers,
                pad,
                physical,
                declared: None,
                chord: DisconnectChord::default(),
                dial: PadDial::default(),
                path,
                serial,
                uniq,
                extras: Default::default(),
            },
        );
        self.slots.get(at)
    }

    /// Brings the table in line with what SDL has attached now: drops pads that went away and
    /// opens ones that arrived. For entering a loop, since a loop that was not running — the
    /// connect wait, the launch animation — consumed or never saw the hotplug events.
    pub(super) fn sync(&mut self, subsystem: &sdl3::GamepadSubsystem) {
        self.slots.retain(|slot| {
            let attached = slot.pad.connected();
            if !attached {
                tracing::info!("controller gone while unwatched: pad {}", slot.index);
            }
            attached
        });
        for id in subsystem.gamepads().unwrap_or_default() {
            self.add(subsystem, id);
        }
    }

    /// Closes the slot for instance `id`. `None` for anything not held — the Magic Remote drops
    /// and re-adds constantly.
    pub(super) fn remove(&mut self, id: JoystickId) -> Option<Slot> {
        let i = self.slots.iter().position(|s| s.id == id)?;
        let slot = self.slots.remove(i);
        tracing::info!("controller disconnected: pad {}", slot.index);
        Some(slot)
    }

    /// The slot for SDL instance `id`.
    pub(super) fn get_mut(&mut self, id: JoystickId) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| s.id == id)
    }

    /// The slot on wire pad `pad`, as the host's feedback planes name it.
    pub(super) fn wire_mut(&mut self, pad: u16) -> Option<&mut Slot> {
        self.slots.iter_mut().find(|s| u16::from(s.index) == pad)
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter()
    }

    pub(super) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Slot> {
        self.slots.iter_mut()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether any pad has held the disconnect/quit chord for `hold`.
    pub(super) fn chord_held(&self, hold: std::time::Duration) -> bool {
        self.slots.iter().any(|s| s.chord.held_for(hold))
    }

    /// Forgets every pad's held chord, so a shortcut fires once per hold.
    pub(super) fn clear_chords(&mut self) {
        for slot in &mut self.slots {
            slot.chord.clear();
        }
    }

    /// The first pad this client recognizes — what `Automatic` mirrors.
    pub(super) fn detected_type(&self) -> Option<GamepadType> {
        self.slots.iter().find_map(|s| s.physical)
    }

    /// What the menus list, in player order.
    pub(super) fn detected(&self) -> Vec<crate::app::DetectedPad> {
        self.slots
            .iter()
            .map(|s| crate::app::DetectedPad {
                name: s.pad.name().unwrap_or_default(),
                kind: s.physical,
                index: s.index,
            })
            .collect()
    }

    /// Every pad as the shared shell lists it, each named by the kind the host builds it as.
    pub(super) fn pad_infos(
        &self,
        setting: GamepadType,
    ) -> impl Iterator<Item = pf_client_core::menu_nav::PadInfo> + '_ {
        self.slots.iter().map(move |s| {
            crate::app::DetectedPad {
                name: s.pad.name().unwrap_or_default(),
                kind: Some(s.host_kind(setting)),
                index: s.index,
            }
            .pad_info()
        })
    }

    /// The pad the single-pad surfaces describe (the legend): player 1.
    pub(super) fn first(&self) -> Option<&Slot> {
        self.slots.first()
    }
}

/// Which pad a touchpad or motion node belongs to: `(uniq, wire index)` per slot, shared with the
/// evdev reader thread.
pub(super) type PadRoutes = std::sync::Arc<std::sync::Mutex<Vec<(Option<String>, u8)>>>;

impl Pads {
    /// Publishes the current slots to `routes`, after any add or remove.
    pub(super) fn publish_routes(&self, routes: &PadRoutes) {
        let table = self.slots.iter().map(|s| (s.uniq.clone(), s.index)).collect();
        *routes.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = table;
    }
}

/// Retargets a pad node's rich input at its controller's slot. `None` drops it: a node whose
/// controller is not unambiguously one slot must not move another player's pad.
pub(super) fn route_rich(
    routes: &PadRoutes,
    mut rich: punktfunk_core::quic::RichInput,
    uniq: Option<&str>,
) -> Option<punktfunk_core::quic::RichInput> {
    use punktfunk_core::quic::RichInput;
    let table = routes.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let index = uniq
        .and_then(|uniq| table.iter().find(|(u, _)| u.as_deref() == Some(uniq)).map(|(_, i)| *i))
        .or_else(|| match table.as_slice() {
            // A lone pad stands in only when one side cannot say who it is: a known, different MAC
            // is an unplugged pad's late sample, not this player's.
            [(slot_uniq, only)] if uniq.is_none() || slot_uniq.is_none() => Some(*only),
            _ => None,
        })?;
    if let RichInput::Touchpad { pad, .. } | RichInput::Motion { pad, .. } = &mut rich {
        *pad = index;
    }
    Some(rich)
}

/// A controller's `Uniq`: off its evdev node, off its hidraw node, or SDL's HIDAPI serial.
fn device_uniq(path: Option<&str>, serial: Option<&str>) -> Option<String> {
    match path {
        Some(p) if p.starts_with("/dev/input/") => crate::platform::webos::evdev::node_uniq(p),
        Some(p) if p.starts_with("/dev/hidraw") => crate::platform::webos::hidraw::Hidraw::uniq_of(p),
        _ => None,
    }
    .or_else(|| serial.and_then(crate::platform::webos::dualsense::mac_from_serial))
}
