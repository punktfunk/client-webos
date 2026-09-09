//! The rim of the frosted card: the `SkSL` and the one function that builds its shader.
//!
//! Split out of [`super::glass`] because it is the only part of the material written in
//! another language — the shader source stays whole and adjacent to the uniforms it declares.

use pf_console_ui::theme;
use skia_safe::{Canvas, RRect, Rect};

/// How far in from the edge the rim ramp reaches, design units. Past ~40 it covers enough of
/// the card to read as shading rather than as an edge; kept narrow so it reads as a chamfer.
const CARD_BEVEL: f32 = 14.0;
/// The RIM of the liquid-glass material, ported from plx-native's `shaders/fs_glass.frag`.
///
/// THE SDF THE ROUNDED RECT ALREADY NEEDS IS THE WHOLE EFFECT. The distance that rounds the
/// corners is exactly the "how deep into the bevel am I" term, and the analytic gradient of
/// the same field is the surface normal to light along.
///
/// Two of plx's three parts are here:
///  - A DIRECTIONAL EDGE LIGHT, not a uniform ring. A slab lit from above has a bright top
///    chamfer and a dark bottom one, and that asymmetry is most of what says "solid object at
///    an angle" rather than "rectangle with a glow". The lit side mixes toward white, the
///    shaded side toward black — a LERP, not an addition, because an added near-black rim
///    adds near-nothing and a dark edge could then never exist.
///  - A HAIRLINE SPECULAR, measured in pixels off the edge rather than as a fraction of the
///    bevel. Its thinness is most of why it reads as an edge rather than as lighting.
///
/// NOT here: the lens, plx's part 1 — the refraction that displaces the backdrop sample
/// outward along the normal. That one needs the blurred backdrop as a shader INPUT, and the
/// only way to get it (`SkImageFilters::RuntimeShader` as the layer's backdrop) sampled to
/// transparent in this app's coordinate space, which `mix(0, frost, .72)` turned into a solid
/// frost-coloured card — the "not transparent" regression. This draws OVER the frost instead
/// and samples nothing, so it cannot reintroduce that.
const CARD_RIM_SKSL: &str = r#"
uniform float2 u_c;      // card centre, canvas px
uniform float2 u_ch;     // card half-size
uniform float  u_r;      // corner radius
uniform float  u_bevel;  // how far in from the edge the rim ramp reaches

// Deliberately faint: the rim states the card's edge, the frost states the material.
const float2 LIGHT = float2(-0.35, -0.94);
const float EDGE = 0.03;
const float DARK = 0.02;
const float SPEC = 0.04;

// Rounded-box SDF. The same `q` also gives the analytic gradient, so the normal is derived
// from it at the one place that needs it rather than returned from here.
float sdBox(float2 q, float r) {
  return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

half4 main(float2 coord) {
  float2 p = coord - u_c;
  float2 q = abs(p) - u_ch + float2(u_r);
  float d = sdBox(q, u_r);
  // One range test covers both dead zones: the flat middle past the bevel, and everything
  // outside the antialiased edge. The branch is coherent across a tile, which is what pays.
  if (d < -u_bevel || d >= 1.0) { return half4(0.0); }

  float white = smoothstep(-2.2, -0.9, d) * (1.0 - smoothstep(-0.6, 0.4, d)) * SPEC;
  float dark = 0.0;

  // The chamfer band peaks just INSIDE the rim: on it, coverage eats most of it and the line
  // reads as aliasing instead of as a highlight. Outside the band no normal is needed, which
  // is what keeps the normalize off the hairline-only and antialiasing pixels.
  float t = 1.0 + d / u_bevel;
  float band = smoothstep(0.55, 1.0, t) * (1.0 - smoothstep(0.94, 1.0, t));
  if (band > 0.0) {
    float2 m = max(q, 0.0);
    float2 n = (q.x > 0.0 || q.y > 0.0)
        ? normalize(m + float2(1e-5))
        : (q.x > q.y ? float2(1.0, 0.0) : float2(0.0, 1.0));
    float ndl = dot(sign(p) * n, LIGHT);
    white += band * max(ndl, 0.0) * EDGE;
    dark = band * max(-ndl, 0.0) * DARK;
  }

  white = min(white, 1.0);
  float a = min(white + dark, 1.0);
  // Premultiplied, so the lit/shaded split IS the colour: rgb = (white / a) * a = white.
  float cov = 1.0 - smoothstep(-1.0, 1.0, d);
  return half4(half3(white * cov), half(a * cov));
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
            skia_safe::RuntimeEffect::make_for_shader(CARD_RIM_SKSL, None)
                .inspect_err(|e| tracing::warn!("card rim shader failed to compile: {e}"))
                .ok()
        })
        .clone()
    })?;
    let mut b = skia_safe::runtime_effect::RuntimeShaderBuilder::new(effect);
    b.set_uniform_float("u_c", &[rect.center_x(), rect.center_y()])
        .and(b.set_uniform_float("u_ch", &[rect.width() / 2.0, rect.height() / 2.0]))
        .and(b.set_uniform_float("u_r", &[corner * k]))
        .and(b.set_uniform_float("u_bevel", &[CARD_BEVEL * k]))
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
