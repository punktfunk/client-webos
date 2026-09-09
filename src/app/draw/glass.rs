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

/// The page as it stood before any modal drew, already blurred by [`blur_page`], for
/// [`glass_card`].
///
/// A snapshot rather than `save_layer`'s `backdrop` filter, which is the obvious answer and did
/// nothing here: the layer was composited but came back unfiltered, so the card read as
/// transparent-but-sharp. An explicit image is also the only form the lens could ever use,
/// since a refraction has to SAMPLE the backdrop at a displaced coordinate.
///
/// Blurred once by whoever takes the snapshot rather than per draw: the page behind a settled
/// modal does not change, and re-running a 14-unit blur over the whole card every frame was
/// most of what the card cost.
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
/// How much smaller everything is blurred than it is drawn. A `CARD_BLUR` gaussian destroys
/// detail far finer than a quarter-resolution downscale does, so the two are indistinguishable
/// while the blur covers a sixteenth of the pixels — and it is the difference between a modal
/// appearing at once and visibly arriving, since a full 1080p blur cost hundreds of
/// milliseconds on this chip. Both callers draw the result stretched back over the source rect.
const DOWNSCALE: i32 = 4;

/// `img` blurred into a quarter-size copy of itself on `target`, which is what decides whether
/// the work lands on the GPU or the CPU: a GPU offscreen for the page, a raster one for a cover
/// small enough that the deferred GPU filter would cost more than the blur.
fn downscaled_blur(
    target: impl FnOnce(i32, i32) -> Option<skia_safe::Surface>,
    img: &skia_safe::Image,
    sigma: f32,
) -> Option<skia_safe::Image> {
    let (w, h) = ((img.width() / DOWNSCALE).max(1), (img.height() / DOWNSCALE).max(1));
    let mut surface = target(w, h)?;
    let mut p = theme::layer();
    // Sigma comes down with the image, so the blur covers the same fraction of it.
    let sigma = sigma / DOWNSCALE as f32;
    p.set_image_filter(skia_safe::image_filters::blur(
        (sigma, sigma),
        skia_safe::TileMode::Clamp,
        None,
        None,
    )?);
    surface
        .canvas()
        .draw_image_rect_with_sampling_options(img, None, Rect::from_iwh(w, h), super::linear(), &p);
    Some(surface.image_snapshot())
}

/// The page, downscaled and blurred once, ready to be a [`Backdrop`].
///
/// `surface` is the live one, only to spawn a compatible offscreen to downscale into: a surface
/// made from it shares the GPU context, so this never leaves the device.
pub(crate) fn blur_page(surface: &mut skia_safe::Surface, page: &skia_safe::Image, k: f32) -> Option<skia_safe::Image> {
    let info = surface.image_info();
    downscaled_blur(
        |w, h| surface.new_surface(&info.with_dimensions((w, h))),
        page,
        CARD_BLUR * k,
    )
}

fn draw_card_backdrop(f: &Frame<'_>, bd: Backdrop<'_>, rr: RRect) {
    let canvas = f.canvas;
    let p = theme::layer();
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
    // Linear: the page is a quarter-size blur, so it comes back up 4x and default sampling
    // would stair-step it.
    canvas.draw_image_rect_with_sampling_options(
        bd.page,
        None,
        Rect::from_xywh(origin.0, origin.1, f.w, f.h),
        super::linear(),
        &p,
    );
    canvas.restore();
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
    let Some(bd) = f.backdrop else {
        canvas.draw_rrect(rr, &theme::fill(surface()));
        theme::panel(canvas, rect, corner, None, PanelStroke::Gradient, k);
        return;
    };
    draw_card_backdrop(f, bd, rr);
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
/// registered through the card's zoom. `false` when the blur could not be baked, and the caller
/// draws the strip opaque.
///
/// The blurred cover is cached under `id`: the menu holds still once it is up, and re-blurring
/// the same cover every frame is what made it lag on the way in.
pub(crate) fn frost_over_art(
    canvas: &Canvas,
    id: &str,
    img: &skia_safe::Image,
    art: Rect,
    window: Rect,
    k: f32,
) -> bool {
    let Some(blurred) = blurred_cover(id, img, art, k) else {
        return false;
    };
    canvas.draw_image_rect_with_sampling_options(&blurred, None, art, super::linear(), &theme::layer());
    let mut face = surface();
    face.a = CARD_FROST;
    canvas.draw_rect(window, &theme::fill(face));
    true
}

/// The one cover the open card menu sits on, blurred. One entry, because one card menu is open
/// at a time.
///
/// The blur is baked at the SOURCE image's scale so it looks the same once drawn into `art`, and
/// the sigma is quantized because `art` is the focus-zoomed, pop-animated card rect: keyed on the
/// exact float, every sub-pixel of that animation would re-bake, which is the cost this cache
/// exists to avoid. A blur drawn stretched cannot show the rounding.
fn blurred_cover(id: &str, img: &skia_safe::Image, art: Rect, k: f32) -> Option<skia_safe::Image> {
    thread_local! {
        static COVER: std::cell::RefCell<Option<(String, i32, skia_safe::Image)>> =
            const { std::cell::RefCell::new(None) };
    }
    if art.width() <= 0.0 {
        return None;
    }
    // Sigma in the source image's pixels: the canvas blur is CARD_BLUR * k across a card of
    // `art.width()`, and the image is squeezed into that, so it scales by the same ratio.
    let sigma = (CARD_BLUR * k * img.width() as f32 / art.width()).round() as i32;
    COVER.with(|c| {
        let mut c = c.borrow_mut();
        if !matches!(&*c, Some((cached, s, _)) if cached == id && *s == sigma) {
            // Raster rather than a GPU offscreen: the source is already a raster image and a
            // quarter-size cover is microseconds on the CPU, where a GPU filter would instead
            // land, deferred, on the flush of the frame the menu opened.
            *c = downscaled_blur(|w, h| skia_safe::surfaces::raster_n32_premul((w, h)), img, sigma as f32)
                .map(|blurred| (id.to_owned(), sigma, blurred));
        }
        c.as_ref().map(|(_, _, img)| img.clone())
    })
}
