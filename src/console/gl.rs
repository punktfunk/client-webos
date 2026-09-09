//! Skia's GLES backend on the app's own SDL window.
//!
//! The window is already `.opengl()` (the stream clears it transparent for NDL's plane), so the
//! console puts a SECOND GL context on it rather than a second window. GL state is per-context,
//! so nothing drawn here can disturb the state SDL's renderer caches for its own — and SDL
//! re-makes that context current on the next `Canvas` draw, which is what hands the screen back
//! when the flip returns to the old menus.

use anyhow::{anyhow, Result};
use skia_safe::gpu::{self, DirectContext, SurfaceOrigin};
use skia_safe::{ColorType, Surface};

const GL_RGBA8: u32 = 0x8058; // RGBA8888 framebuffer format

/// Skia's resource budget. A quarter of the desktop's 160 MB: this `SoC` shares one memory pool
/// with the NDL decoder, and covers are the only large thing the console caches.
pub(crate) const GPU_CACHE_BYTES: usize = 64 << 20;

pub(crate) struct ConsoleGl {
    /// Held for the process rather than per menu entry. Dropping it throws away every compiled
    /// shader and the glyph atlas, and a cold shell spends 372 ms rebuilding them across its
    /// first ~90 frames (`design/webos-console-port-handoff.md` §1) — once is a splash screen,
    /// once per menu entry is a stutter every time a stream ends.
    ctx: sdl2::video::GLContext,
    context: DirectContext,
    /// Skia over framebuffer 0; cached to avoid re-wrapping on every frame.
    surface: Option<(Surface, u32, u32)>,
    /// What the window's config actually granted, not what was asked for — Skia must be told
    /// the truth or it clips paths against a buffer that is not there.
    stencil: usize,
}

impl ConsoleGl {
    pub(crate) fn new(window: &sdl2::video::Window, video: &sdl2::VideoSubsystem) -> Result<Self> {
        let ctx = window
            .gl_create_context()
            .map_err(|e| anyhow!("console GL context: {e}"))?;
        // Paces the console on the panel rather than spinning: `gl_swap_window` is this loop's
        // only sleep. Best-effort — a driver that refuses just leaves the frame budget below.
        if let Err(e) = video.gl_set_swap_interval(sdl2::video::SwapInterval::VSync) {
            tracing::debug!("console: no vsync swap interval ({e}) — pacing on the frame budget");
        }
        let attr = video.gl_attr();
        let stencil = attr.stencil_size() as usize;
        tracing::info!(
            "console: GL context up — {:?}, alpha {} bits, stencil {stencil} bits",
            attr.context_profile(),
            attr.alpha_size(),
        );
        // Through SDL's resolver, not Skia's `new_native()`: the native assembler has no reason
        // to find webOS's loader, and SDL already knows it (proven by `tools/webos-glprobe`).
        let interface =
            gpu::gl::Interface::new_load_with(|name| video.gl_get_proc_address(name) as *const std::ffi::c_void)
                .ok_or_else(|| anyhow!("Skia: no GL interface from SDL's resolver"))?;
        let mut context = gpu::direct_contexts::make_gl(interface, None)
            .ok_or_else(|| anyhow!("Skia: DirectContext over GLES failed"))?;
        context.set_resource_cache_limit(GPU_CACHE_BYTES);
        tracing::info!("console: Skia GL context, {} MB budget", GPU_CACHE_BYTES >> 20);
        Ok(Self {
            ctx,
            context,
            surface: None,
            stencil,
        })
    }

    /// Called on every console entry; old menus and the stream have both made their own
    /// context current in between.
    pub(crate) fn make_current(&self, window: &sdl2::video::Window) -> Result<()> {
        window
            .gl_make_current(&self.ctx)
            .map_err(|e| anyhow!("console: gl_make_current: {e}"))
    }

    /// Re-wraps on drawable size change to keep Skia from clipping against a stale buffer.
    pub(crate) fn surface(&mut self, w: u32, h: u32) -> Result<&mut Surface> {
        if !matches!(self.surface, Some((_, sw, sh)) if sw == w && sh == h) {
            self.surface = None;
            let fb = gpu::gl::FramebufferInfo {
                fboid: 0,
                format: GL_RGBA8,
                protected: gpu::Protected::No,
            };
            let target = gpu::backend_render_targets::make_gl((w as i32, h as i32), None, self.stencil, fb);
            let surface = gpu::surfaces::wrap_backend_render_target(
                &mut self.context,
                &target,
                SurfaceOrigin::BottomLeft,
                ColorType::RGBA8888,
                None,
                None,
            )
            .ok_or_else(|| anyhow!("Skia: could not wrap the window framebuffer ({w}x{h})"))?;
            tracing::info!("console: drawing at {w}x{h}");
            self.surface = Some((surface, w, h));
        }
        // The branch above either returned an error or filled it.
        let surface = &mut self.surface.as_mut().expect("just wrapped").0;
        // Handed out at identity. The surface is cached by size and this context is shared by
        // both menu flows and the stream overlays, so a canvas carries whatever matrix the last
        // one left on it — the pointer UI scales by the panel correction, which silently cropped
        // the gamepad shell until this reset existed. Resetting here rather than in each drawer
        // is what keeps the next flow from having to know that.
        surface.canvas().reset_matrix();
        Ok(surface)
    }

    /// SDL still owns the swap.
    pub(crate) fn flush(&mut self) {
        self.context.flush_and_submit();
    }

    /// Hand the covers and glyph atlases back before a stream takes the GPU. The context and
    /// its compiled shaders stay, so returning to the console costs a re-upload, not the cold
    /// shader warm-up `ctx`'s doc describes.
    pub(crate) fn release_resources(&mut self) {
        self.surface = None;
        self.context.free_gpu_resources();
    }
}
