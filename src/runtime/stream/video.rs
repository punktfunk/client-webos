use std::ffi::{c_void, CStr};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use skia_safe::gpu::{self, gl, Mipmapped, SurfaceOrigin, YUVABackendTextures};
use skia_safe::{yuva_info, Canvas, Paint, Rect, YUVAInfo, YUVColorSpace};

use crate::platform::webos::pyro;

const GL_TEXTURE_2D: u32 = 0x0DE1;
const GL_TEXTURE_MIN_FILTER: u32 = 0x2801;
const GL_TEXTURE_MAG_FILTER: u32 = 0x2800;
const GL_TEXTURE_WRAP_S: u32 = 0x2802;
const GL_TEXTURE_WRAP_T: u32 = 0x2803;
const GL_LINEAR: i32 = 0x2601;
const GL_CLAMP_TO_EDGE: i32 = 0x812F;
const GL_UNPACK_ALIGNMENT: u32 = 0x0CF5;
const GL_UNPACK_ROW_LENGTH: u32 = 0x0CF2;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const GL_RED: u32 = 0x1903;
const GL_R8: u32 = 0x8229;
const GL_LUMINANCE: u32 = 0x1909;
const GL_LUMINANCE8: u32 = 0x8040;
const GL_VERSION: u32 = 0x1F02;

type GetString = unsafe extern "C" fn(u32) -> *const u8;
type GenTextures = unsafe extern "C" fn(i32, *mut u32);
type DeleteTextures = unsafe extern "C" fn(i32, *const u32);
type BindTexture = unsafe extern "C" fn(u32, u32);
type TexParameteri = unsafe extern "C" fn(u32, u32, i32);
type TexImage = unsafe extern "C" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);
type PixelStorei = unsafe extern "C" fn(u32, i32);

struct Gl {
    gen_textures: GenTextures,
    delete_textures: DeleteTextures,
    bind_texture: BindTexture,
    tex_parameteri: TexParameteri,
    tex_image_2d: TexImage,
    tex_sub_image_2d: TexImage,
    pixel_storei: PixelStorei,
    es3: bool,
}

fn gl_fns() -> Option<&'static Gl> {
    static GL: OnceLock<Option<Gl>> = OnceLock::new();
    GL.get_or_init(|| {
        let load = |name: &CStr| unsafe { sdl3::sys::video::SDL_GL_GetProcAddress(name.as_ptr()) };
        macro_rules! f {
            ($name:literal, $ty:ty) => {
                unsafe { std::mem::transmute::<unsafe extern "C" fn(), $ty>(load($name)?) }
            };
        }
        let get_string = f!(c"glGetString", GetString);
        let version = unsafe { get_string(GL_VERSION) };
        let version = if version.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(version.cast()) }.to_string_lossy().into_owned()
        };
        let es3 = version.starts_with("OpenGL ES 3");
        tracing::info!("pyrowave: GL upload path on {version:?} (R8 planes: {es3})");
        Some(Gl {
            gen_textures: f!(c"glGenTextures", GenTextures),
            delete_textures: f!(c"glDeleteTextures", DeleteTextures),
            bind_texture: f!(c"glBindTexture", BindTexture),
            tex_parameteri: f!(c"glTexParameteri", TexParameteri),
            tex_image_2d: f!(c"glTexImage2D", TexImage),
            tex_sub_image_2d: f!(c"glTexSubImage2D", TexImage),
            pixel_storei: f!(c"glPixelStorei", PixelStorei),
            es3,
        })
    })
    .as_ref()
}

struct Planes {
    generation: u64,
    ids: [u32; 3],
    dims: [(i32, i32); 3],
    textures: Vec<gpu::BackendTexture>,
}

impl Planes {
    fn new(gl: &Gl, output: &pyro::Output) -> Self {
        let (w, h) = (output.width as i32, output.height as i32);
        let dims = [(w, h), (w / 2, h / 2), (w / 2, h / 2)];
        let (internal, format, sized) = if gl.es3 {
            (GL_R8, GL_RED, GL_R8)
        } else {
            (GL_LUMINANCE, GL_LUMINANCE, GL_LUMINANCE8)
        };
        let mut ids = [0u32; 3];
        unsafe {
            (gl.gen_textures)(3, ids.as_mut_ptr());
            for (id, (pw, ph)) in ids.iter().zip(dims) {
                (gl.bind_texture)(GL_TEXTURE_2D, *id);
                for (k, v) in [
                    (GL_TEXTURE_MIN_FILTER, GL_LINEAR),
                    (GL_TEXTURE_MAG_FILTER, GL_LINEAR),
                    (GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE),
                    (GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE),
                ] {
                    (gl.tex_parameteri)(GL_TEXTURE_2D, k, v);
                }
                (gl.tex_image_2d)(
                    GL_TEXTURE_2D,
                    0,
                    internal as i32,
                    pw,
                    ph,
                    0,
                    format,
                    GL_UNSIGNED_BYTE,
                    std::ptr::null(),
                );
            }
        }
        let textures = ids
            .iter()
            .zip(dims)
            .map(|(id, d)| unsafe {
                gpu::backend_textures::make_gl(
                    d,
                    Mipmapped::No,
                    gl::TextureInfo {
                        target: GL_TEXTURE_2D,
                        id: *id,
                        format: sized,
                        protected: gpu::Protected::No,
                    },
                    "pyrowave plane",
                )
            })
            .collect();
        Self {
            generation: output.generation,
            ids,
            dims,
            textures,
        }
    }

    fn upload(&self, gl: &Gl, planes: [(*const u8, u32); 3]) {
        let format = if gl.es3 { GL_RED } else { GL_LUMINANCE };
        unsafe {
            (gl.pixel_storei)(GL_UNPACK_ALIGNMENT, 1);
            for ((id, (w, h)), (ptr, pitch)) in self.ids.iter().zip(self.dims).zip(planes) {
                if gl.es3 {
                    (gl.pixel_storei)(GL_UNPACK_ROW_LENGTH, pitch as i32);
                }
                (gl.bind_texture)(GL_TEXTURE_2D, *id);
                (gl.tex_sub_image_2d)(GL_TEXTURE_2D, 0, 0, 0, w, h, format, GL_UNSIGNED_BYTE, ptr.cast());
            }
            if gl.es3 {
                (gl.pixel_storei)(GL_UNPACK_ROW_LENGTH, 0);
            }
        }
    }

    fn release(self, gl: &Gl) {
        drop(self.textures);
        unsafe { (gl.delete_textures)(3, self.ids.as_ptr()) };
    }
}

#[derive(Default)]
pub(super) struct Video {
    drawn: Option<(u64, u64)>,
    planes: Option<Planes>,
    upload: pyro::Timing,
    draw: pyro::Timing,
    age: pyro::Timing,
    interval: pyro::Timing,
    last_drawn: Option<Instant>,
}

impl Video {
    pub(super) fn poll(&self) -> Option<bool> {
        let output = pyro::current_output()?;
        if output.vo {
            return None;
        }
        let (_, seq) = output.latest()?;
        Some(self.drawn != Some((output.generation, seq)))
    }

    pub(super) fn draw(&mut self, canvas: &Canvas, w: f32, h: f32) {
        let Some(output) = pyro::current_output() else {
            return;
        };
        let Some((slot, seq)) = output.latest() else {
            return;
        };
        let Some(gl) = gl_fns() else {
            return;
        };
        let Some(mut context) = canvas.direct_context() else {
            return;
        };
        let space = if output.full_range {
            YUVColorSpace::Rec709_Full
        } else {
            YUVColorSpace::Rec709_Limited
        };
        let Some(info) = YUVAInfo::new(
            (output.width as i32, output.height as i32),
            yuva_info::PlaneConfig::Y_U_V,
            yuva_info::Subsampling::S420,
            space,
            None,
            None,
        ) else {
            return;
        };
        if self.planes.as_ref().is_some_and(|p| p.generation != output.generation) {
            if let Some(old) = self.planes.take() {
                old.release(gl);
            }
        }
        let planes = self.planes.get_or_insert_with(|| Planes::new(gl, &output));
        output.mark_shown(slot);
        let age = output.age(slot);
        let started = Instant::now();
        planes.upload(gl, output.planes(slot));
        context.reset(Some(
            (gl::BackendState::TEXTURE_BINDING | gl::BackendState::PIXEL_STORE).bits(),
        ));
        let uploaded = started.elapsed();
        let Some(textures) = YUVABackendTextures::new(&info, &planes.textures, SurfaceOrigin::TopLeft) else {
            return;
        };
        let Some(image) = gpu::images::texture_from_yuva_textures(&mut context, &textures, None) else {
            tracing::warn!("pyrowave: Skia could not wrap the planes");
            return;
        };
        canvas.draw_image_rect(&image, None, Rect::from_wh(w, h), &Paint::default());
        self.upload.add("pyrowave gl upload", uploaded);
        self.draw.add("pyrowave skia draw record", started.elapsed() - uploaded);
        self.age.add("pyrowave decoded->drawn", age + started.elapsed());
        if let Some(last) = self.last_drawn.replace(Instant::now()) {
            self.interval
                .add("pyrowave draw interval", last.elapsed().min(Duration::from_secs(1)));
        }
        self.drawn = Some((output.generation, seq));
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        if let (Some(planes), Some(gl)) = (self.planes.take(), gl_fns()) {
            planes.release(gl);
        }
    }
}
