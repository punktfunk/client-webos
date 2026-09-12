//! Shared frosted-glass modal material, ported from plx-native’s `shaders/fs_glass.frag`.
//! Frames without a backdrop use an opaque card, including frames over live video.

use pf_console_ui::theme::{self, PanelStroke};
use skia_safe::{Canvas, RRect, Rect};

use super::card_material;
use super::{surface, Frame};

/// Preblurred page snapshot shared by modal draws. Explicit snapshots avoid the
/// unfiltered output observed with `save_layer` backdrop filters.
#[derive(Clone, Copy)]
pub(crate) struct Backdrop<'a> {
    pub page: &'a skia_safe::Image,
}

/// Blur sigma in design units, scaled by `k`.
const CARD_BLUR: f32 = 28.0;
const CARD_FROST: f32 = 0.88;
/// The blurred backdrop lifts the face, so it needs less tint than the opaque card.
const CARD_TINT: f32 = 0.10;
/// Half resolution keeps blur cost down without the visible interpolation contours
/// of quarter resolution. Grain dithers quantization, but cannot hide those contours.
const DOWNSCALE: i32 = 2;

/// The target chooses GPU storage for pages or raster storage for small covers.
fn downscaled_blur(
    target: impl FnOnce(i32, i32) -> Option<skia_safe::Surface>,
    img: &skia_safe::Image,
    sigma: f32,
) -> Option<skia_safe::Image> {
    let (w, h) = ((img.width() / DOWNSCALE).max(1), (img.height() / DOWNSCALE).max(1));
    let mut surface = target(w, h)?;
    let mut p = theme::layer();
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

/// Blur into a compatible offscreen surface to keep the page on the GPU.
pub(crate) fn blur_page(surface: &mut skia_safe::Surface, page: &skia_safe::Image, k: f32) -> Option<skia_safe::Image> {
    let info = surface.image_info();
    downscaled_blur(
        |w, h| surface.new_surface(&info.with_dimensions((w, h))),
        page,
        CARD_BLUR * k,
    )
}

fn frosted_face() -> skia_safe::Color4f {
    skia_safe::Color4f {
        a: CARD_FROST,
        ..theme::card_face(CARD_TINT)
    }
}

fn draw_card_backdrop(f: &Frame<'_>, bd: Backdrop<'_>, rr: RRect) {
    let canvas = f.canvas;
    let p = theme::layer();
    canvas.save();
    canvas.clip_rrect(rr, None, Some(true));
    // Use both frame dimensions for nonuniform display scaling. Cancel modal-rise
    // translation so the backdrop stays registered with the page during animation.
    let m = canvas.local_to_device_as_3x3();
    let (sx, sy) = (m.scale_x(), m.scale_y());
    let origin = if sx == 0.0 || sy == 0.0 {
        (0.0, 0.0)
    } else {
        (-m.translate_x() / sx, -m.translate_y() / sy)
    };
    canvas.draw_image_rect_with_sampling_options(
        bd.page,
        None,
        Rect::from_xywh(origin.0, origin.1, f.w, f.h),
        super::linear(),
        &p,
    );
    canvas.restore();
}

/// The rim shader lights the chamfer; this stroke defines the edge.
fn card_hairline(canvas: &Canvas, rect: Rect, rr: RRect, k: f32) {
    let mut p = theme::shaded_stroke(k);
    p.set_shader(skia_safe::gradient::shaders::linear_gradient(
        (
            skia_safe::Point::new(rect.left, rect.top),
            skia_safe::Point::new(rect.left, rect.bottom),
        ),
        &skia_safe::gradient::Gradient::new(
            skia_safe::gradient::Colors::new_evenly_spaced(
                &[theme::fg(0.18), theme::fg(0.035)],
                skia_safe::TileMode::Clamp,
                None,
            ),
            skia_safe::gradient::Interpolation::default(),
        ),
        None,
    ));
    canvas.draw_rrect(rr, &p);
}

pub(crate) fn glass_card(f: &Frame<'_>, rect: Rect, corner: f32) {
    let (canvas, k) = (f.canvas, f.k);
    let rr = RRect::new_rect_xy(rect, corner * k, corner * k);
    let Some(bd) = f.backdrop else {
        canvas.draw_rrect(rr, &theme::fill(surface()));
        theme::panel(canvas, rect, corner, None, PanelStroke::Gradient, k);
        return;
    };
    draw_card_backdrop(f, bd, rr);
    canvas.draw_rrect(rr, &theme::fill(frosted_face()));
    // theme::panel would re-opacify the translucent face.
    card_material::draw(canvas, rr, rect, corner, k);
    card_hairline(canvas, rect, rr, k);
}

/// Frost the cover in its original rect to preserve registration during zoom.
/// Returns false when blur fails so the caller can draw an opaque strip.
pub(crate) fn frost_over_art(canvas: &Canvas, img: &skia_safe::Image, art: Rect, window: Rect, k: f32) -> bool {
    let Some(blurred) = blurred_cover(img, art, k) else {
        return false;
    };
    canvas.draw_image_rect_with_sampling_options(&blurred, None, art, super::linear(), &theme::layer());
    canvas.draw_rect(window, &theme::fill(frosted_face()));
    true
}

/// One cached cover for the single open card menu. Quantize source-space sigma
/// to avoid rebuilding the blur for every subpixel of the card's zoom animation.
fn blurred_cover(img: &skia_safe::Image, art: Rect, k: f32) -> Option<skia_safe::Image> {
    thread_local! {
        static COVER: std::cell::RefCell<Option<(u32, i32, skia_safe::Image)>> =
            const { std::cell::RefCell::new(None) };
    }
    if art.width() <= 0.0 {
        return None;
    }
    let sigma = (CARD_BLUR * k * img.width() as f32 / art.width()).round() as i32;
    COVER.with(|c| {
        let mut c = c.borrow_mut();
        if !matches!(&*c, Some((image_id, s, _)) if *image_id == img.unique_id() && *s == sigma) {
            // Raster blur avoids deferring GPU filter work to the menu's opening frame.
            *c = downscaled_blur(|w, h| skia_safe::surfaces::raster_n32_premul((w, h)), img, sigma as f32)
                .map(|blurred| (img.unique_id(), sigma, blurred));
        }
        c.as_ref().map(|(_, _, img)| img.clone())
    })
}
