//! Shared frosted-glass modal material, ported from plx-native’s `shaders/fs_glass.frag`.
//! Frames without a backdrop use an opaque card, including frames over live video.

use pf_console_ui::theme::{self, PanelStroke};
use skia_safe::{Canvas, RRect, Rect};

use super::card_material;
use super::{surface, Frame};

/// Blur sigma in design units, scaled by `k`.
const CARD_BLUR: f32 = 28.0;
const CARD_FROST: f32 = 0.88;
/// The blurred backdrop lifts the face, so it needs less tint than the opaque card.
const CARD_TINT: f32 = 0.10;
/// Half resolution keeps blur cost down without the visible interpolation contours
/// of quarter resolution. Grain dithers quantization, but cannot hide those contours.
const DOWNSCALE: i32 = 2;

/// The sigma a card's page backdrop is blurred at: [`CARD_BLUR`] at the layout's scale, then at
/// the drawable's. One rule, so the startup warmup compiles the blur the menu actually draws.
pub(crate) fn page_sigma(layout_h: u32, drawable_h: u32) -> f32 {
    CARD_BLUR * super::scale(layout_h) * drawable_h as f32 / layout_h.max(1) as f32
}

/// Blur `image` at half resolution into an offscreen compatible with `canvas`, so a GPU page
/// stays on the GPU. The offscreen is not pooled: the snapshot outlives the call by frames, and
/// drawing into a surface it still references would cost the full copy-on-write this avoids.
pub(crate) fn blur_image(canvas: &Canvas, image: &skia_safe::Image, sigma: f32) -> Option<skia_safe::Image> {
    let (w, h) = ((image.width() / DOWNSCALE).max(1), (image.height() / DOWNSCALE).max(1));
    let mut surface = canvas.new_surface(&canvas.image_info().with_dimensions((w, h)), None)?;
    let sigma = sigma / DOWNSCALE as f32;
    let mut p = theme::layer();
    p.set_image_filter(skia_safe::image_filters::blur(
        (sigma, sigma),
        skia_safe::TileMode::Clamp,
        None,
        None,
    )?);
    surface
        .canvas()
        .draw_image_rect_with_sampling_options(image, None, Rect::from_iwh(w, h), super::linear(), &p);
    Some(surface.image_snapshot())
}

fn frosted_face() -> skia_safe::Color4f {
    skia_safe::Color4f {
        a: CARD_FROST,
        ..theme::card_face(CARD_TINT)
    }
}

fn draw_card_backdrop(f: &Frame<'_>, page: &skia_safe::Image, rr: RRect) {
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
        page,
        None,
        Rect::from_xywh(origin.0, origin.1, f.w, f.h),
        super::linear(),
        &p,
    );
    canvas.restore();
}

/// The rim shader lights the chamfer; this stroke defines the edge.
fn card_hairline(canvas: &Canvas, rr: RRect, k: f32) {
    let rect = rr.rect();
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
    let Some(page) = f.backdrop else {
        canvas.draw_rrect(rr, &theme::fill(surface()));
        theme::panel(canvas, rect, corner, None, PanelStroke::Gradient, k);
        return;
    };
    draw_card_backdrop(f, page, rr);
    draw_face(canvas, rr, k);
}

/// The translucent face over whatever backdrop the caller has already laid down.
fn draw_face(canvas: &Canvas, rr: RRect, k: f32) {
    canvas.draw_rrect(rr, &theme::fill(frosted_face()));
    // theme::panel would re-opacify the translucent face.
    card_material::draw(canvas, rr, k);
    card_hairline(canvas, rr, k);
}

/// Frost the cover in its original rect to preserve registration during zoom.
/// Returns false when blur fails so the caller can draw an opaque strip.
pub(crate) fn frost_over_art(canvas: &Canvas, img: &skia_safe::Image, art: Rect, window: Rect, k: f32) -> bool {
    let Some(blurred) = blurred_cover(canvas, img, art, k) else {
        return false;
    };
    canvas.draw_image_rect_with_sampling_options(&blurred, None, art, super::linear(), &theme::layer());
    // The caller clips the strip to the cover's rounded outer corners.
    draw_face(canvas, RRect::new_rect(window), k);
    true
}

thread_local! {
    static COVER: std::cell::RefCell<Option<(u32, i32, skia_safe::Image)>> =
        const { std::cell::RefCell::new(None) };
}

pub(crate) fn clear_cover() {
    COVER.with(|c| *c.borrow_mut() = None);
}

/// Quantize source-space sigma so subpixel zoom steps reuse the single cached cover.
fn blurred_cover(canvas: &Canvas, img: &skia_safe::Image, art: Rect, k: f32) -> Option<skia_safe::Image> {
    if art.width() <= 0.0 {
        return None;
    }
    let sigma = (CARD_BLUR * k * img.width() as f32 / art.width()).round() as i32;
    COVER.with(|c| {
        let mut c = c.borrow_mut();
        if !matches!(&*c, Some((image_id, s, _)) if *image_id == img.unique_id() && *s == sigma) {
            *c = blur_image(canvas, img, sigma as f32).map(|blurred| (img.unique_id(), sigma, blurred));
        }
        c.as_ref().map(|(_, _, img)| img.clone())
    })
}
