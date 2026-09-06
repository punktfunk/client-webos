//! The two-button confirm dialog, as a screen — the shell every one of them draws.
//!
//! What differs between Forget host, Send logs, a Wake prompt and a finished speed test is a
//! title and a [`Confirm`]; the card, the header and the button row are this. A speed test
//! still running has no buttons yet, so it keeps its own `Modal` and takes them from the same
//! descriptor once it does.
use crate::app::screens::confirm::Confirm;
use crate::ui;
use crate::ui::render::Rect;
use crate::ui::text::Fonts;
use crate::ui::Canvas;
use crate::ui::ModalMetrics;
use crate::ui::ModalScreen;
use anyhow::Result;

pub(crate) struct Modal<'a> {
    pub title: &'a str,
    pub confirm: &'a Confirm,
}

impl ModalMetrics for Modal<'_> {
    fn card_rect(&self, screen_w: u32, screen_h: u32, fonts: &Fonts) -> Rect {
        ui::tiles::confirm_dialog_card(screen_w, screen_h, fonts, &self.confirm.subtitle)
    }
}

impl ModalScreen for Modal<'_> {
    fn render(&self, c: &mut Canvas, hover_close: bool) -> Result<()> {
        c.confirm_dialog(
            self.title,
            &self.confirm.subtitle,
            ui::theme::palette().muted,
            &self.confirm.widgets(),
            hover_close,
            ui::tiles::ConfirmSurface::Glass,
        )
    }
}
