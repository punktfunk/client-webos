//! Frosted-card surface lighting and its shader uniforms.

use pf_console_ui::theme;
use skia_safe::{Canvas, RRect, Rect};

/// Broad, faint edge lighting in design units.
const CARD_BEVEL: f32 = 20.0;
/// Drawn over the frost without sampling the backdrop; backdrop shader sampling previously
/// returned transparent pixels in this coordinate space.
const CARD_SKSL: &str = r#"
uniform float2 u_c;
uniform float2 u_inner;
uniform float  u_r;
// CPU-computed reciprocals of bevel width and card height.
uniform float  u_ibevel;
uniform float  u_iheight;

const float2 LIGHT = float2(-0.35, -0.94);
const float EDGE = 0.016;
const float DARK = 0.010;
const float SHEEN = 0.028;
const float FLOOR = 0.030;
// Dither masks banding in the upscaled 8-bit backdrop and face gradient.
const float GRAIN = 0.024;

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
  return half4(half3(white * cov), half((white + dark) * cov));
}
"#;

/// Cached per thread because `RuntimeEffect` is not `Send`.
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
    let radius = corner * k;
    let mut b = skia_safe::runtime_effect::RuntimeShaderBuilder::new(effect);
    b.set_uniform_float("u_c", &[rect.center_x(), rect.center_y()])
        .and(b.set_uniform_float("u_inner", &[rect.width() / 2.0 - radius, rect.height() / 2.0 - radius]))
        .and(b.set_uniform_float("u_r", &[radius]))
        .and(b.set_uniform_float("u_ibevel", &[1.0 / (CARD_BEVEL * k)]))
        .and(b.set_uniform_float("u_iheight", &[1.0 / rect.height().max(1.0)]))
        .ok()?;
    b.make_shader(&skia_safe::Matrix::default())
}

pub(super) fn draw(canvas: &Canvas, rr: RRect, rect: Rect, corner: f32, k: f32) {
    thread_local! {
        static SHADER: std::cell::RefCell<Option<(Rect, f32, f32, skia_safe::Shader)>> =
            const { std::cell::RefCell::new(None) };
    }
    let shader = SHADER.with(|cache| {
        let mut cache = cache.borrow_mut();
        if !matches!(&*cache, Some((r, c, scale, _)) if *r == rect && *c == corner && *scale == k) {
            *cache = shader(rect, corner, k).map(|shader| (rect, corner, k, shader));
        }
        cache.as_ref().map(|(_, _, _, shader)| shader.clone())
    });
    if let Some(shader) = shader {
        let mut p = theme::shaded();
        p.set_shader(shader);
        canvas.draw_rrect(rr, &p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn material_compiles_and_accepts_tv_uniforms() {
        for k in [0.5, 1.0, 2.0] {
            assert!(shader(Rect::from_xywh(30.0, 40.0, 800.0 * k, 600.0 * k), 20.0, k).is_some());
        }
    }
}
