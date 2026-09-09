//! The frosted-glass material every modal card is made of, ported from plx-native's
//! `shaders/fs_glass.frag`.
//!
//! One seam: all five modal painters call [`glass_card`], so the material lives in one
//! function rather than in each of them. A card frosts only when its [`Frame`] carries a
//! [`Backdrop`] — the page as it stood before any modal drew. Without one it is the flat
//! opaque card it has always been, which is what a frame over live video must stay.

use pf_console_ui::theme::{self, PanelStroke};
use skia_safe::{Canvas, RRect, Rect};

use super::card_rim;
use super::{surface, Frame};

/// The page as it stood before any modal drew, for [`glass_card`].
///
/// A snapshot rather than `save_layer`'s `backdrop` filter, which is the obvious answer and did
/// nothing here: the layer was composited but came back unfiltered, so the card read as
/// transparent-but-sharp. An explicit image is also the only form the lens could ever use,
/// since a refraction has to SAMPLE the backdrop at a displaced coordinate.
#[derive(Clone, Copy)]
pub(crate) struct Backdrop<'a> {
    pub page: &'a skia_safe::Image,
}

/// How far the backdrop is smeared under a frosted card, in design units (scaled by `k`).
const CARD_BLUR: f32 = 14.0;
/// Frost opacity. plx-native measured 0.72 as the legibility floor on a television: more
/// transparent shows more backdrop and costs the contrast a couch reader needs, and the
/// binding constraint is the text on the card, not the material. Tuned denser than that.
const CARD_FROST: f32 = 0.9;
/// The card blur, cached per `k` to avoid allocating and busting Skia's filter cache every frame.
fn card_blur(k: f32) -> Option<skia_safe::ImageFilter> {
    thread_local! {
        static BLUR: std::cell::RefCell<Option<(f32, skia_safe::ImageFilter)>> =
            const { std::cell::RefCell::new(None) };
    }
    BLUR.with(|c| {
        let mut c = c.borrow_mut();
        if !matches!(&*c, Some((cached, _)) if *cached == k) {
            let sigma = CARD_BLUR * k;
            *c = skia_safe::image_filters::blur((sigma, sigma), skia_safe::TileMode::Clamp, None, None).map(|f| (k, f));
        }
        c.as_ref().map(|(_, f)| f.clone())
    })
}

/// Blur the page under the card if a blur filter exists; return `false` to signal the caller to draw opaque.
fn draw_card_backdrop(f: &Frame<'_>, bd: Backdrop<'_>, rr: RRect) -> bool {
    let canvas = f.canvas;
    let Some(blur) = card_blur(f.k) else {
        return false;
    };
    let mut p = theme::layer();
    p.set_image_filter(blur);
    canvas.save();
    canvas.clip_rrect(rr, None, Some(true));
    // The page covers the whole surface, and the whole surface in layout units is the frame:
    // taking the size from the frame rather than from the image keeps the two in step on both
    // axes, which a single device-pixels-per-unit ratio cannot when the drawable's aspect is
    // not the display mode's. The clip is what bounds the work to the card.
    //
    // Placed against the surface, not against the card: every painter translates by the modal
    // rise before it gets here, so drawing at the frame's origin would slide the backdrop down
    // with the card and show the page off-register for the whole open animation. The CTM's own
    // translation, back in layout units, is exactly what has to come off again.
    let m = canvas.local_to_device_as_3x3();
    let (sx, sy) = (m.scale_x(), m.scale_y());
    let origin = if sx == 0.0 || sy == 0.0 {
        (0.0, 0.0)
    } else {
        (-m.translate_x() / sx, -m.translate_y() / sy)
    };
    canvas.draw_image_rect(bd.page, None, Rect::from_xywh(origin.0, origin.1, f.w, f.h), &p);
    canvas.restore();
    true
}

/// The card's top-to-bottom hairline: the flat panel's stroke, kept over the glass because the
/// rim shader lights the chamfer but does not draw the edge itself.
fn card_hairline(canvas: &Canvas, rect: Rect, rr: RRect, k: f32) {
    let mut p = theme::shaded_stroke(k);
    p.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            skia_safe::Point::new(rect.left, rect.top),
            skia_safe::Point::new(rect.left, rect.bottom),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &[theme::fg(0.22), theme::fg(0.04)],
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rrect(rr, &p);
}

/// A raised card. Over the menu it is a pane of frosted glass: the page behind it blurred,
/// under a translucent face, with [`CARD_RIM_SKSL`]'s chamfer light and hairline over that.
/// Without a backdrop on the frame it stays the opaque face it has always been.
pub(crate) fn glass_card(f: &Frame<'_>, rect: Rect, corner: f32) {
    let (canvas, k) = (f.canvas, f.k);
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    let frosted = f.backdrop.is_some_and(|bd| draw_card_backdrop(f, bd, rr));
    if !frosted {
        canvas.draw_rrect(rr, &theme::fill(surface()));
        theme::panel(canvas, rect, corner, None, PanelStroke::Gradient, k);
        return;
    }
    // Face must be translucent so the blurred backdrop shows through.
    let mut face = surface();
    face.a = CARD_FROST;
    canvas.draw_rrect(rr, &theme::fill(face));
    // Skip theme::panel on frosted path: its glass fill would re-opacify the translucent face.
    card_rim::draw(canvas, rr, rect, corner, k);
    card_hairline(canvas, rect, rr, k);
}

/// Frost `window` over the cover already drawn at `art`, same blur and face as [`glass_card`].
///
/// The card menu grows out of one card, so the only thing behind it is that card's cover:
/// blurring the image straight into the rect it was drawn at needs no surface grab and stays
/// registered through the card's zoom. `false` when there is no blur filter.
pub(crate) fn frost_over_art(canvas: &Canvas, img: &skia_safe::Image, art: Rect, window: Rect, k: f32) -> bool {
    let Some(blur) = card_blur(k) else {
        return false;
    };
    let mut p = theme::layer();
    p.set_image_filter(blur);
    canvas.draw_image_rect(img, None, art, &p);
    let mut face = surface();
    face.a = CARD_FROST;
    canvas.draw_rect(window, &theme::fill(face));
    true
}
