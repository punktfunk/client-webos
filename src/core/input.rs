use punktfunk_core::input::{InputEvent, InputKind};

/// Dedupes key/button edges across input sources sharing the host's input devices.
#[derive(Default)]
pub struct HeldInputs {
    held: Vec<(u32, InputEvent)>,
}

impl HeldInputs {
    pub fn is_edge(event: &InputEvent) -> bool {
        matches!(
            event.kind,
            InputKind::KeyDown | InputKind::KeyUp | InputKind::MouseButtonDown | InputKind::MouseButtonUp
        )
    }

    pub fn send(&mut self, source: u32, event: &InputEvent, mut send: impl FnMut(&InputEvent)) {
        let down = matches!(event.kind, InputKind::KeyDown | InputKind::MouseButtonDown);
        let key = matches!(event.kind, InputKind::KeyDown | InputKind::KeyUp);
        let same = |held: &InputEvent| held.code == event.code && matches!(held.kind, InputKind::KeyDown) == key;
        let index = self
            .held
            .iter()
            .position(|(owner, held)| *owner == source && same(held));
        if down {
            if index.is_none() {
                self.held.push((source, *event));
            }
            send(event);
        } else if let Some(index) = index {
            self.held.swap_remove(index);
            if !self.held.iter().any(|(_, held)| same(held)) {
                send(event);
            }
        }
    }

    pub fn release(&mut self, source: u32, mut send: impl FnMut(&InputEvent)) {
        while let Some(index) = self.held.iter().position(|(owner, _)| *owner == source) {
            let mut event = self.held[index].1;
            event.kind = if matches!(event.kind, InputKind::KeyDown) {
                InputKind::KeyUp
            } else {
                InputKind::MouseButtonUp
            };
            self.send(source, &event, &mut send);
        }
    }
}
