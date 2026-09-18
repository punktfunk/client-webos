//! Frosted-card surface lighting and its shader uniforms.

use pf_console_ui::theme;
use skia_safe::{Canvas, RRect, Rect};

/// Rim width in design units, never under one device pixel.
const RIM_WIDTH: f32 = 1.0;
/// How far the absorbed colour bleeds inward from the rim, and how wide each sample spreads
/// along it, in design units.
const GLOW_DEPTH: f32 = 4.0;
const GLOW_SPREAD: f32 = 9.0;
/// Accent share of the face colour the rim mixes in; the card's own face tint is too faint to read.
const RIM_FACE_TINT: f32 = 0.6;
/// Broad, faint edge lighting in design units.
const CARD_BEVEL: f32 = 20.0;
/// Drawn over the frost. The rim samples the sharp page under it, so the edge takes the colour
/// of what it cuts across, like a glass edge.
const CARD_SKSL: &str = r#"
uniform shader u_page;
uniform float2 u_c;
uniform float2 u_inner;
uniform float  u_r;
// CPU-computed reciprocals of bevel width and card height.
uniform float  u_ibevel;
uniform float  u_iheight;
// Half a device pixel in local units and the pixel's reciprocal: the rim's antialiasing footprint.
uniform float  u_hpx;
uniform float  u_ipx;
uniform float  u_rim;
// Reciprocal of the glow depth.
uniform float  u_iglow;
uniform float  u_spread;
// The modal's own face colour, which the rim carries alongside the page's.
uniform float3 u_face;

const float2 LIGHT = float2(-0.35, -0.94);
const float EDGE = 0.016;
const float DARK = 0.010;
const float SHEEN = 0.028;
const float FLOOR = 0.030;
// Dither masks banding in the upscaled 8-bit backdrop and face gradient. Roughly three
// 8-bit levels peak-to-peak: enough to break a step, below where it reads as texture.
const float GRAIN = 0.012;
// The rim lifts the page colour under it and fades from top to bottom.
// The lift keeps the edge a highlight over dark page; the gain carries the page's hue into it.
const float RIM_GAIN = 1.15;
const float RIM_LIFT = 0.04;
const float RIM_TOP = 0.85;
const float RIM_BOTTOM = 0.40;
// Share of the rim colour taken from the modal face rather than the page.
const float RIM_FACE = 0.40;
// Peak opacity of the colour diffusing into the face just inside the rim.
const float GLOW = 0.30;

// White noise avoids the diagonal patterns of interleaved gradient noise.
float hash(float2 c) {
  float3 f = fract(c.xyx * float3(0.1031, 0.1030, 0.0973));
  f += dot(f, f.yzx + 33.33);
  return fract((f.x + f.y) * f.z);
}

half4 main(float2 coord) {
  float2 p = coord - u_c;
  float2 q = abs(p) - u_inner;
  float mq = max(q.x, q.y);
  bool corner = min(q.x, q.y) > 0.0;
  float cornerLength = corner ? length(q) : 0.0;
  float d = (corner ? cornerLength : mq) - u_r;
  if (d >= 1.0) { return half4(0.0); }

  // Split signed lighting into white and black contributions for premultiplied output.
  float v = smoothstep(0.0, 1.0, 0.5 + p.y * u_iheight);
  float g = (hash(coord) - 0.5) * GRAIN;
  float white = SHEEN * (1.0 - v) * (1.0 - v) + max(g, 0.0);
  float dark = FLOOR * v * v + max(-g, 0.0);

  // Skip edge lighting and coverage calculations across the flat interior.
  float t = 1.0 + d * u_ibevel;
  float cov = 1.0;
  if (t > 0.0) {
    // Peak inside the rim to avoid attenuating the highlight with edge coverage.
    float band = smoothstep(0.35, 1.0, t) * (1.0 - smoothstep(0.90, 1.0, t));
    if (band > 0.0) {
      // Blend adjoining edge lights without boosting the curved contour.
      float ndl = corner
        ? dot(sign(p) * q, LIGHT) / (q.x + q.y)
        : (q.x > q.y ? sign(p.x) * LIGHT.x : sign(p.y) * LIGHT.y);
      white += band * max(ndl, 0.0) * EDGE;
      dark += band * max(-ndl, 0.0) * DARK;
    }
    cov = 1.0 - smoothstep(-1.0, 1.0, d);
  }

  // Lighting amplitudes keep premultiplied alpha below 1 without clamping.
  half4 base = half4(half3(white * cov), half((white + dark) * cov));

  if (-d * u_iglow >= 1.0) { return base; }

  // Three taps along the edge smear what the rim catches instead of copying the pixel under
  // it. Bilinear filtering between the wide taps stands in for a denser kernel.
  float2 n = corner
    ? sign(p) * q / max(cornerLength, 1e-4)
    : (q.x > q.y ? float2(sign(p.x), 0.0) : float2(0.0, sign(p.y)));
  float2 e = coord - n * d;
  float2 along = float2(-n.y, n.x) * u_spread;
  half3 page = saturate((u_page.eval(e).rgb * 2.0
    + u_page.eval(e + along).rgb + u_page.eval(e - along).rgb) * (0.25 * RIM_GAIN));
  half fade = half(mix(RIM_TOP, RIM_BOTTOM, v));

  // The absorbed colour diffuses into the face with a quadratic falloff.
  float x = 1.0 - saturate(-d * u_iglow);
  half ga = half(GLOW * x * x * cov) * fade;
  half4 lit = half4(page * ga, ga) + base * (1.0 - ga);

  // Box-filtered overlap of this pixel with the band d in [-u_rim, 0].
  float rim = saturate((min(0.0, d + u_hpx) - max(-u_rim, d - u_hpx)) * u_ipx);
  if (rim <= 0.0) { return lit; }
  half3 tint = mix(saturate(page + RIM_LIFT), half3(u_face), RIM_FACE);
  half a = half(rim) * fade;
  return half4(tint * a, a) + lit * (1.0 - a);
}
"#;

/// Cached per thread because `RuntimeEffect` is not `Send`. Built relative to the page origin,
/// so a card and page moving together reuse it; a card rising over a still page rebuilds.
fn shader(key: &Key, page: &skia_safe::Image) -> Option<skia_safe::Shader> {
    thread_local! {
        static EFFECT: std::cell::OnceCell<Option<skia_safe::RuntimeEffect>> =
            const { std::cell::OnceCell::new() };
    }
    let effect = EFFECT.with(|c| {
        c.get_or_init(|| {
            skia_safe::RuntimeEffect::make_for_shader(CARD_SKSL, None)
                .inspect_err(|e| tracing::warn!("card material shader failed to compile: {e}"))
                .ok()
        })
        .clone()
    })?;
    let &Key {
        rect,
        radius,
        k,
        px,
        face,
        size,
        ..
    } = key;
    let m = skia_safe::Matrix::rect_2_rect(Rect::from_iwh(page.width(), page.height()), Rect::from_size(size), None)?;
    let page = page.to_shader(
        (skia_safe::TileMode::Clamp, skia_safe::TileMode::Clamp),
        super::linear(),
        &m,
    )?;
    // The builder cannot bind a child shader, so pack the uniforms by their reflected offsets.
    let mut data = vec![0u8; effect.uniform_size()];
    let mut set = |name: &str, v: &[f32]| {
        let at = effect.find_uniform(name)?.offset();
        for (i, f) in v.iter().enumerate() {
            data.get_mut(at + i * 4..at + i * 4 + 4)?
                .copy_from_slice(&f.to_ne_bytes());
        }
        Some(())
    };
    set("u_c", &[rect.center_x(), rect.center_y()])?;
    set("u_inner", &[rect.width() / 2.0 - radius, rect.height() / 2.0 - radius])?;
    set("u_r", &[radius])?;
    set("u_ibevel", &[1.0 / (CARD_BEVEL * k)])?;
    set("u_iheight", &[1.0 / rect.height().max(1.0)])?;
    set("u_hpx", &[px / 2.0])?;
    set("u_ipx", &[1.0 / px])?;
    set("u_rim", &[(RIM_WIDTH * k).max(px)])?;
    set("u_iglow", &[1.0 / (GLOW_DEPTH * k)])?;
    set("u_spread", &[GLOW_SPREAD * k])?;
    set("u_face", &face)?;
    effect.make_shader(skia_safe::Data::new_copy(&data), &[page.into()], None)
}

/// Everything a shader is built from; a match means the cached one is still exact. `rect` is
/// relative to the page's origin.
#[derive(PartialEq)]
struct Key {
    rect: Rect,
    /// Device px.
    radius: f32,
    k: f32,
    px: f32,
    face: [f32; 3],
    page: u32,
    size: skia_safe::Size,
}

type Cached = (Key, skia_safe::Shader);

// A departing card, arriving card, cover strip and quit dialog can coexist.
const CACHE_SLOTS: usize = 4;
thread_local! {
    static SHADER: std::cell::RefCell<[Option<Cached>; CACHE_SLOTS]> =
        const { std::cell::RefCell::new([const { None }; CACHE_SLOTS]) };
}

/// Drop cached shaders. They hold the page and cover images, whose textures
/// `free_gpu_resources` cannot reclaim while referenced.
pub(super) fn release() {
    SHADER.with(|cache| *cache.borrow_mut() = [const { None }; CACHE_SLOTS]);
}

/// Light the face of `rr` and draw its rim from `page`, an image laid over `dst` in local space.
/// The rounded rect carries both the bounds and the corner radius the uniforms need, so there
/// is one source for each rather than a rect-plus-corner pair to keep in step with it.
pub(super) fn draw(canvas: &Canvas, rr: RRect, k: f32, page: &skia_safe::Image, dst: Rect) {
    let origin = (dst.left, dst.top);
    let scale = canvas.local_to_device_as_3x3().scale_y().abs();
    // Keyed, so a theme change rebuilds the shader.
    let face = theme::card_face(RIM_FACE_TINT);
    let key = Key {
        rect: rr.rect().with_offset((-origin.0, -origin.1)),
        radius: rr.simple_radii().x,
        k,
        px: if scale > 0.0 { 1.0 / scale } else { 1.0 },
        face: [face.r, face.g, face.b],
        page: page.unique_id(),
        size: dst.size(),
    };
    let shader = SHADER.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(at) = cache
            .iter()
            .position(|s| s.as_ref().is_some_and(|(cached, _)| *cached == key))
        {
            cache[..=at].rotate_right(1);
        } else {
            cache.rotate_right(1);
            cache[0] = shader(&key, page).map(|shader| (key, shader));
        }
        cache[0].as_ref().map(|(_, shader)| shader.clone())
    });
    if let Some(shader) = shader {
        let mut p = theme::shaded();
        p.set_shader(shader.with_local_matrix(&skia_safe::Matrix::translate(origin)));
        canvas.draw_rrect(rr, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_compiles_and_accepts_tv_uniforms() {
        for k in [0.5, 1.0, 2.0] {
            let page = skia_safe::surfaces::raster_n32_premul((64, 36))
                .unwrap()
                .image_snapshot();
            let key = Key {
                rect: Rect::from_xywh(30.0, 40.0, 800.0 * k, 600.0 * k),
                radius: 20.0 * k,
                k,
                px: 1.0,
                face: [0.5; 3],
                page: page.unique_id(),
                size: skia_safe::Size::new(1920.0, 1080.0),
            };
            assert!(shader(&key, &page).is_some());
        }
    }
}
