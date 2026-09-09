//! The lit surface of the frosted card — its rim and its face: the `SkSL` and the one function
//! that builds its shader.
//!
//! Split out of [`super::glass`] because it is the only part of the material written in
//! another language — the shader source stays whole and adjacent to the uniforms it declares.

use pf_console_ui::theme;
use skia_safe::{Canvas, RRect, Rect};

/// How far in from the edge the rim ramp reaches, design units. Past ~40 it covers enough of
/// the card to read as shading rather than as an edge; kept narrow so it reads as a chamfer.
const CARD_BEVEL: f32 = 14.0;
/// The lit surface of the liquid-glass material, ported from plx-native's `shaders/fs_glass.frag`.
///
/// THE SDF THE ROUNDED RECT ALREADY NEEDS IS THE WHOLE EFFECT. The distance that rounds the
/// corners is exactly the "how deep into the bevel am I" term, and the analytic gradient of
/// the same field is the surface normal to light along.
///
/// Three parts make it up:
///  - A DIRECTIONAL EDGE LIGHT, not a uniform ring. A slab lit from above has a bright top
///    chamfer and a dark bottom one, and that asymmetry is most of what says "solid object at
///    an angle" rather than "rectangle with a glow". The lit side goes white, the shaded side
///    black, so a dark edge can exist at all — an added near-black rim adds near-nothing.
///  - A HAIRLINE SPECULAR, a couple of pixels off the edge.
///  - A DIFFUSION GRADIENT over the whole face, with a GRAIN over it. The backdrop under the
///    card comes back up from a downscaled 8-bit blur, so its smooth regions land as visible
///    steps; a dither of a couple of levels is what breaks them, and the same noise is what
///    keeps this gradient from banding in its turn.
///
/// NOT here: the lens, plx's part 1 — the refraction that displaces the backdrop sample
/// outward along the normal. That one needs the blurred backdrop as a shader INPUT, and the
/// only way to get it (`SkImageFilters::RuntimeShader` as the layer's backdrop) sampled to
/// transparent in this app's coordinate space, which `mix(0, frost, .72)` turned into a solid
/// frost-coloured card — the "not transparent" regression. This draws OVER the frost instead
/// and samples nothing, so it cannot reintroduce that.
const CARD_SKSL: &str = r#"
uniform float2 u_c;
uniform float2 u_ch;
uniform float  u_r;
// Reciprocals inverted on the CPU rather than divided here: 1/bevel for the depth ramp,
// and 1/(2*u_ch.y) for the face gradient.
uniform float  u_ibevel;
uniform float  u_iheight;

// Deliberately faint: the rim states the card's edge, the frost states the material.
const float2 LIGHT = float2(-0.35, -0.94);
const float EDGE = 0.02;
const float DARK = 0.015;
const float SPEC = 0.025;
// The face: a top-lit sheen and a floor shade, wide enough to read as diffusion through the
// material rather than as a second edge.
const float SHEEN = 0.03;
const float FLOOR = 0.04;
// Dither amplitude, peak half this. About two 8-bit levels: enough to dissolve the upscaled
// backdrop's steps, under the threshold where it reads as grain in its own right on a
// television — which is where a wider one lands, contouring or not.
const float GRAIN = 0.024;

// White noise rather than an interleaved gradient: IGN's structure rules the flat face
// diagonally, and on a card this large that is its own artefact.
float hash(float2 c) {
  float3 f = fract(c.xyx * float3(0.1031, 0.1030, 0.0973));
  f += dot(f, f.yzx + 33.33);
  return fract((f.x + f.y) * f.z);
}

half4 main(float2 coord) {
  float2 p = coord - u_c;
  float2 q = abs(p) - u_ch + u_r;
  // Rounded-box SDF, but the sqrt only where a corner needs it: inside both edges the distance
  // IS the larger coordinate, and that is the whole face. The same `q` is the analytic
  // gradient, so the normal below costs nothing beyond a normalize.
  float mq = max(q.x, q.y);
  float d = (mq < 0.0 ? mq : length(max(q, 0.0))) - u_r;
  if (d >= 1.0) { return half4(0.0); }

  // Every term is a signed amount of light split into a white part and a black part: a
  // premultiplied shader cannot emit negative light, and the split is also what hazes the face,
  // since the alpha is the sum and the colour only the white.
  //
  // Squared on both sides so the ramp is long and neither end states an edge of its own.
  float v = clamp(0.5 + p.y * u_iheight, 0.0, 1.0);
  // The dither rides it, with no sign branch: per-pixel divergence buys nothing over two maxes.
  float g = (hash(coord) - 0.5) * GRAIN;
  float white = SHEEN * (1.0 - v) * (1.0 - v) + max(g, 0.0);
  float dark = FLOOR * v * v + max(-g, 0.0);

  // Past the bevel the card is flat face, and neither the rim terms nor the edge coverage
  // reach it. That branch is what keeps the smoothsteps off the face; it is coherent across a
  // tile, which is what pays for it.
  float t = 1.0 + d * u_ibevel;
  float cov = 1.0;
  if (t > 0.0) {
    // A hairline specular in PIXELS off the edge, not as a fraction of the bevel: its thinness
    // is most of why it reads as an edge rather than as lighting.
    white += smoothstep(-2.2, -0.9, d) * (1.0 - smoothstep(-0.6, 0.4, d)) * SPEC;

    // The chamfer band peaks just INSIDE the rim: on it, coverage eats most of it and the line
    // reads as aliasing instead of as a highlight. Outside the band no normal is needed, which
    // keeps the normalize off the hairline-only and antialiasing pixels.
    float band = smoothstep(0.55, 1.0, t) * (1.0 - smoothstep(0.94, 1.0, t));
    if (band > 0.0) {
      // On a straight edge the gradient is the axis itself; only corners need normalizing.
      float2 n = mq > 0.0 ? normalize(max(q, 0.0) + 1e-5) : (q.x > q.y ? float2(1.0, 0.0) : float2(0.0, 1.0));
      float ndl = dot(sign(p) * n, LIGHT);
      white += band * max(ndl, 0.0) * EDGE;
      dark += band * max(-ndl, 0.0) * DARK;
    }
    cov = 1.0 - smoothstep(-1.0, 1.0, d);
  }

  // No clamp: every constant above is hundredths, so the sum cannot reach 1.
  return half4(half3(white * cov), half((white + dark) * cov));
}
"#;

/// Compiled once per thread — `RuntimeEffect` is refcounted but not `Send`, so this cannot be
/// a `OnceLock`. Each draw clones a handle rather than recompiling `SkSL`.
fn shader(rect: Rect, corner: f32, k: f32) -> Option<skia_safe::Shader> {
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
    let mut b = skia_safe::runtime_effect::RuntimeShaderBuilder::new(effect);
    b.set_uniform_float("u_c", &[rect.center_x(), rect.center_y()])
        .and(b.set_uniform_float("u_ch", &[rect.width() / 2.0, rect.height() / 2.0]))
        .and(b.set_uniform_float("u_r", &[corner * k]))
        .and(b.set_uniform_float("u_ibevel", &[1.0 / (CARD_BEVEL * k)]))
        .and(b.set_uniform_float("u_iheight", &[1.0 / rect.height().max(1.0)]))
        .ok()?;
    b.make_shader(&skia_safe::Matrix::default())
}

/// A no-op if the shader fails to compile: graceful degradation to a chamferless card.
pub(super) fn draw(canvas: &Canvas, rr: RRect, rect: Rect, corner: f32, k: f32) {
    if let Some(shader) = shader(rect, corner, k) {
        let mut p = theme::shaded();
        p.set_shader(shader);
        canvas.draw_rrect(rr, &p);
    }
}
