//! Per-screen app-state logic (event handling, state transitions). Split out of the
//! former fused `app/<screen>.rs` modules — see `docs/APP-REWORK-PLAN.md`.
//! Rendering counterparts live in `app::view`.
mod about;
pub(crate) mod addhost;
pub(crate) mod cardmenu;
pub(crate) mod collections;
mod edithost;
mod forget;
pub(crate) mod hdrcalibration;
mod home;
pub(crate) mod hostmenu;
pub(crate) mod hostpower;
mod pairing;
pub(crate) mod profilepick;
pub(crate) mod profiles;
pub(crate) mod reach;
pub(crate) mod sendlogs;
pub(crate) mod settingspage;
pub(crate) mod speedtest;
pub(crate) mod textfield;
mod wake;

use crate::app::App;

impl App {
    /// Everything an open screen changes *by itself*, with no event behind it — a wake probe
    /// landing, a pattern feed starting to present or giving up. Returns whether any of it moved
    /// pixels, which is the loop's whole interest in it.
    pub(crate) fn tick_screens(&mut self) -> bool {
        let mut changed = self.tick_wake();
        changed |= self.tick_hdr_pattern();
        changed
    }

    /// Whether this frame composes only what belongs *over* the video plane: no sidebar, no
    /// grid, no status block, no full-screen scrim. Two ways in — a screen that draws over a
    /// running stream (`screens::over_video`), and the launch backdrop dissolving into the
    /// picture it was covering (`app::hero`).
    ///
    /// Deliberately not [`Self::video_underlay`]: a calibration pattern that has not started
    /// presenting yet still owns the screen, and composing the menu back in behind its card
    /// for those seconds — undimmed, since `compose_modal` skips the scrim over video — is a
    /// flash of the whole home screen that then vanishes when the first frame lands.
    pub(crate) fn over_video_layers(&self) -> bool {
        self.render.hero.dissolving() || crate::app::screens::over_video(self.nav.screen)
    }

    /// Whether the video plane is what the user is looking at this frame, with the graphics
    /// plane an overlay on top of it — i.e. whether the frame is punched through to it.
    ///
    /// [`Self::over_video_layers`] and a plane actually carrying a picture: clearing
    /// transparent with nothing behind it is a black screen, so the pattern's feed has to be
    /// presenting before the hole is opened.
    pub(crate) fn video_underlay(&self) -> bool {
        self.render.hero.dissolving()
            || (crate::app::screens::over_video(self.nav.screen) && self.hdr_pattern_presenting())
    }

    /// What the frame is cleared to before the draw list runs.
    ///
    /// Transparent over the video plane: NDL is an *underlay*, and the menu's opaque
    /// background is what normally hides it. Clearing transparent instead leaves the graphics
    /// plane carrying only what is drawn over the picture.
    pub(crate) fn frame_clear_color(&self) -> skia_safe::Color4f {
        if self.video_underlay() {
            skia_safe::Color4f::new(0.0, 0.0, 0.0, 0.0)
        } else {
            crate::app::draw::ground()
        }
    }
}
