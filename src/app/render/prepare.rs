//! Per-frame bookkeeping before the frame is drawn: the grid's covers and the launch hero.
//! Everything on screen is painted from state each frame (`app::draw`); this pass only
//! readies what painting cannot make on the spot.
use crate::app::App;
use crate::ui::render::Size;

impl App {
    /// Builds the launching game's hero image, once, and starts its fade-in clock. Gated on
    /// the launch having started: at ~1600px wide this is a multi-MB texture, and one for
    /// every card scrolled past would undo the windowed cover cache.
    fn prepare_hero(&mut self) {
        if self.launch_anim.is_none() {
            return;
        }
        let Some(id) = self.render.hero.pending_upload() else {
            return;
        };
        self.render.hero.mark_uploaded(id);
        self.render.hero_image = self.render.hero.uploaded_image().and_then(|hero| {
            crate::app::draw::home::raw_image(
                hero.width,
                hero.height,
                crate::app::draw::home::RawFormat::Rgb565,
                &hero.pixels,
            )
        });
    }

    /// Readies this frame: the grid's cover window and the hero. Call `advance_frame` first.
    pub fn prepare_frame(&mut self, screen: Size) {
        self.prepare_grid(screen);
        self.prepare_hero();
    }
}
