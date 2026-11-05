use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ash::vk;
use ash::vk::Handle as _;
use pyrowave_sys as pw;

use crate::core::media::{VideoSink, VideoSinkCaps};

const RING: usize = 4;
const IN_FLIGHT: u32 = 2;
const NO_SLOT: usize = usize::MAX;

pub struct Output {
    pub width: u32,
    pub height: u32,
    pub generation: u64,
    pub full_range: bool,
    pub vo: bool,
    pub nv12: bool,
    decoded_at: [AtomicU64; RING],
    frames: [[(usize, u32); 3]; RING],
    latest: AtomicUsize,
    seq: AtomicU64,
    shown: AtomicUsize,
    busy: AtomicUsize,
    gpu_ns: AtomicU64,
}

impl Output {
    pub fn latest(&self) -> Option<(usize, u64)> {
        let seq = self.seq.load(Ordering::Acquire);
        let slot = self.latest.load(Ordering::Acquire);
        (slot != NO_SLOT).then_some((slot, seq))
    }

    pub fn planes(&self, slot: usize) -> [(*const u8, u32); 3] {
        self.frames[slot].map(|(ptr, pitch)| (ptr as *const u8, pitch))
    }

    pub fn mark_shown(&self, slot: usize) {
        self.shown.store(slot, Ordering::Release);
    }

    pub fn age(&self, slot: usize) -> Duration {
        Duration::from_nanos(now_ns().saturating_sub(self.decoded_at[slot].load(Ordering::Acquire)))
    }
}

fn now_ns() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[derive(Default)]
pub struct Timing {
    since: Option<Instant>,
    samples: Vec<Duration>,
}

impl Timing {
    pub fn add(&mut self, label: &str, d: Duration) {
        let since = *self.since.get_or_insert_with(Instant::now);
        self.samples.push(d);
        let window = since.elapsed();
        if window >= Duration::from_secs(10) {
            let s = &mut self.samples;
            s.sort_unstable();
            let ms = |d: Duration| d.as_secs_f64() * 1e3;
            let at = |q: usize| ms(s[(s.len() - 1) * q / 100]);
            tracing::info!(
                "{label}: {} frames in {:.1}s, mean {:.2} ms, p50 {:.2} ms, p95 {:.2} ms, max {:.2} ms",
                s.len(),
                window.as_secs_f32(),
                ms(s.iter().sum::<Duration>()) / s.len() as f64,
                at(50),
                at(95),
                at(100)
            );
            *self = Self::default();
        }
    }
}

static ARRIVAL: [AtomicU64; 2] = [AtomicU64::new(0), AtomicU64::new(0)];
static FED: AtomicU64 = AtomicU64::new(0);
static REPLACED: AtomicU64 = AtomicU64::new(0);
static VO_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Packet arrival stamps (CLOCK_REALTIME ns) of the AU the pump is about to feed.
pub fn note_arrival(first_ns: u64, received_ns: u64) {
    ARRIVAL[0].store(first_ns, Ordering::Relaxed);
    ARRIVAL[1].store(received_ns, Ordering::Relaxed);
}

#[derive(Default)]
struct RxTimes {
    assembly: Timing,
    handoff: Timing,
}

struct Census {
    at: Instant,
    ticks: std::collections::HashMap<String, u64>,
    cores: Vec<(u64, u64)>,
    counts: [u64; 3],
    irq: [u64; 4],
}

impl Census {
    fn new() -> Self {
        let mut c = Self { at: Instant::now(), ticks: Default::default(), cores: Vec::new(), counts: [0; 3], irq: Self::irq() };
        (c.ticks, c.cores) = {
            let (t, k) = c.read();
            (t.into_iter().map(|(n, t, _)| (n, t)).collect(), k)
        };
        c
    }

    fn irq() -> [u64; 4] {
        let l = std::fs::read_to_string("/proc/stat").unwrap_or_default();
        let v: Vec<u64> = l.lines().next().unwrap_or("").split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
        let g = |i| v.get(i).copied().unwrap_or(0);
        [g(0) + g(1), g(2), g(5), g(6)]
    }

    fn read(&mut self) -> (Vec<(String, u64, String)>, Vec<(u64, u64)>) {
        let mut threads = Vec::new();
        if let Ok(dir) = std::fs::read_dir("/proc/self/task") {
            for e in dir.flatten() {
                let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
                let (Some(l), Some(r)) = (stat.find('('), stat.rfind(')')) else { continue };
                let f: Vec<&str> = stat[r + 2..].split_whitespace().collect();
                let ticks = f.get(11).zip(f.get(12)).and_then(|(u, s)| Some(u.parse::<u64>().ok()? + s.parse::<u64>().ok()?));
                if let Some(t) = ticks {
                    let tid = e.file_name().to_string_lossy().into_owned();
                    threads.push((format!("{}/{tid}", &stat[l + 1..r]), t, f.get(36).copied().unwrap_or("?").to_owned()));
                }
            }
        }
        let mut cores = Vec::new();
        if let Ok(stat) = std::fs::read_to_string("/proc/stat") {
            for line in stat.lines().filter(|l| l.starts_with("cpu") && !l.starts_with("cpu ")) {
                let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
                let total: u64 = v.iter().take(8).sum();
                let idle = v.get(3).unwrap_or(&0) + v.get(4).unwrap_or(&0);
                cores.push((total - idle.min(total), total));
            }
        }
        (threads, cores)
    }

    fn tick(&mut self) {
        let el = self.at.elapsed();
        if el < Duration::from_secs(10) {
            return;
        }
        let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as f64;
        let (threads, cores) = self.read();
        let mut rows: Vec<(f64, String)> = threads
            .iter()
            .filter_map(|(n, t, c)| {
                let pct = t.saturating_sub(*self.ticks.get(n).unwrap_or(t)) as f64 / hz / el.as_secs_f64() * 100.0;
                (pct >= 1.0).then(|| (pct, format!("{n} {pct:.0}%@c{c}")))
            })
            .collect();
        let irq = Self::irq();
        let names = ["sys.user", "sys.system", "sys.irq", "sys.softirq"];
        for (i, (n, o)) in irq.iter().zip(self.irq).enumerate() {
            let pct = n.saturating_sub(o) as f64 / hz / el.as_secs_f64() * 100.0;
            if pct >= 1.0 {
                rows.push((pct, format!("{}/0 {pct:.0}%@c-", names[i])));
            }
        }
        self.irq = irq;
        rows.sort_by(|a, b| b.0.total_cmp(&a.0));
        let busy: Vec<String> = cores
            .iter()
            .zip(self.cores.iter().chain(std::iter::repeat(&(0, 0))))
            .map(|(n, o)| format!("{:.0}", (n.0.saturating_sub(o.0)) as f64 * 100.0 / n.1.saturating_sub(o.1).max(1) as f64))
            .collect();
        let now = [FED.load(Ordering::Relaxed), REPLACED.load(Ordering::Relaxed), VO_SKIPPED.load(Ordering::Relaxed)];
        tracing::info!(
            "pyrowave threads: {:.1}s, {}; cores busy % [{}]",
            el.as_secs_f32(),
            rows.iter().map(|r| r.1.as_str()).collect::<Vec<_>>().join(", "),
            busy.join("/")
        );
        tracing::info!(
            "pyrowave stale: {} fed, {} replaced before decode, {} skipped before show in {:.1}s",
            now[0] - self.counts[0],
            now[1] - self.counts[1],
            now[2] - self.counts[2],
            el.as_secs_f32()
        );
        self.ticks = threads.into_iter().map(|(n, t, _)| (n, t)).collect();
        self.cores = cores;
        self.counts = now;
        self.at = Instant::now();
    }
}

static OUTPUT: Mutex<Option<Arc<Output>>> = Mutex::new(None);
static HUD: Mutex<Option<Arc<punktfunk_core::hud::Stats>>> = Mutex::new(None);

pub fn set_hud(hud: Arc<punktfunk_core::hud::Stats>) {
    *HUD.lock().unwrap_or_else(PoisonError::into_inner) = Some(hud);
}

fn realtime_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}
static GENERATION: AtomicU64 = AtomicU64::new(0);
pub static DISPLAY: OnceLock<(u32, u32)> = OnceLock::new();
pub static VO_WINDOW: OnceLock<(usize, usize, u32, u32)> = OnceLock::new();

extern "C" {
    fn pf_vo_punch(display: *mut c_void, surface: *mut c_void, context: *const c_char, w: i32, h: i32) -> i32;
    fn pf_vo_open(uhd: i32) -> i32;
    fn pf_vo_show(luma: *const u8, chroma: *const u8, stride: u32, w: u32, h: u32) -> i32;
    fn pf_vo_close();
}

const VO_CONTEXT: &str = "pfvo";

fn app_flag(name: &str) -> bool {
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.parent()?.join(name)))
        .is_some_and(|p| p.exists())
}

fn vo_wanted(width: u32, height: u32) -> bool {
    VO_WINDOW.get().is_some() && !app_flag("pyro-gl") && (width > 1920 || height > 1080)
}

fn vo_luna(bus: &super::ls2::Bus, method: &str, payload: &str) {
    let uri = format!("luna://com.webos.service.videooutput/{method}");
    let (Ok(uri), Ok(payload)) = (std::ffi::CString::new(uri), std::ffi::CString::new(payload)) else {
        return;
    };
    tracing::info!("pyrowave vo: {method}");
    match bus.call(&uri, &payload, super::ls2::Call::Video) {
        Ok(()) => {
            let until = Instant::now() + Duration::from_millis(300);
            while Instant::now() < until {
                bus.pump();
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        Err(e) => tracing::warn!("pyrowave vo: {method} failed: {e:#}"),
    }
}

fn vo_attach(width: u32, height: u32) -> Result<()> {
    let &(display, surface, w, h) = VO_WINDOW.get().context("no window")?;
    let ctx = std::ffi::CString::new(VO_CONTEXT)?;
    let r = unsafe { pf_vo_punch(display as *mut c_void, surface as *mut c_void, ctx.as_ptr(), w as i32, h as i32) };
    if r != 0 {
        bail!("punch-through failed: {r}");
    }
    let r = unsafe { pf_vo_open(i32::from(width > 1920 || height > 1080)) };
    if r != 0 {
        bail!("DILE_VO_Open failed: {r}");
    }
    Ok(())
}

fn vo_connect(bus: &super::ls2::Bus, width: u32, height: u32) {
    let app = super::ndl::app_id();
    vo_luna(bus, "register", &format!(r#"{{"context":"{VO_CONTEXT}","appId":"{app}"}}"#));
    vo_luna(bus, "connect", &format!(r#"{{"context":"{VO_CONTEXT}","source":"JPEG","sourcePort":0,"sink":"MAIN"}}"#));
    vo_luna(
        bus,
        "setVideoData",
        &format!(
            r#"{{"context":"{VO_CONTEXT}","sink":"MAIN","contentType":"photo","frameRate":60,"width":{width},"height":{height},"scanType":"progressive","adaptive":false}}"#
        ),
    );
    vo_luna(bus, "display/setDisplayWindow", &format!(r#"{{"context":"{VO_CONTEXT}","sink":"MAIN","fullScreen":true}}"#));
    vo_luna(
        bus,
        "display/setCompositing",
        &format!(r#"{{"composeOrder":{{"context":"{VO_CONTEXT}","sink":"MAIN","opacity":255,"zOrder":0}}}}"#),
    );
    vo_luna(bus, "blankVideo", &format!(r#"{{"context":"{VO_CONTEXT}","sink":"MAIN","blank":false}}"#));
}

fn vo_detach(bus: &super::ls2::Bus) {
    vo_luna(bus, "disconnect", &format!(r#"{{"context":"{VO_CONTEXT}","sink":"MAIN"}}"#));
    vo_luna(bus, "unregister", &format!(r#"{{"context":"{VO_CONTEXT}"}}"#));
    unsafe { pf_vo_close() };
}

fn interleave(dst: &mut [u8], pitch: usize, cb: (*const u8, u32), cr: (*const u8, u32), w2: usize, h2: usize) {
    for y in 0..h2 {
        let row = &mut dst[y * pitch..y * pitch + 2 * w2];
        let (b, r) = unsafe {
            (
                std::slice::from_raw_parts(cb.0.add(y * cb.1 as usize), w2),
                std::slice::from_raw_parts(cr.0.add(y * cr.1 as usize), w2),
            )
        };
        for (o, (b, r)) in row.chunks_exact_mut(2).zip(b.iter().zip(r)) {
            o[0] = *b;
            o[1] = *r;
        }
    }
}

fn vo_thread(output: Weak<Output>, frames: mpsc::Receiver<usize>) {
    let Some((width, height)) = output.upgrade().map(|o| (o.width, o.height)) else {
        return;
    };
    let bus = match super::ls2::Bus::open_named(c"com.webos.avconnector.punktfunk") {
        Ok(bus) => bus,
        Err(e) => {
            tracing::warn!("pyrowave vo: {e:#}");
            return;
        }
    };
    if let Err(e) = vo_attach(width, height) {
        tracing::warn!("pyrowave vo: {e:#}");
        unsafe { pf_vo_close() };
        return;
    }
    let mut chroma = Vec::new();
    let mut connected = false;
    let mut wake = Timing::default();
    let mut il = Timing::default();
    let mut show = Timing::default();
    let mut age = Timing::default();
    let mut interval = Timing::default();
    let mut last: Option<Instant> = None;
    while let Ok(mut slot) = frames.recv() {
        while let Ok(newer) = frames.try_recv() {
            slot = newer;
            VO_SKIPPED.fetch_add(1, Ordering::Relaxed);
        }
        let Some(out) = output.upgrade() else {
            break;
        };
        out.mark_shown(slot);
        wake.add("pyrowave vo wake", out.age(slot));
        let [(y, pitch), cb, cr] = out.planes(slot);
        let t = Instant::now();
        let uv = if out.nv12 && cb.1 == pitch {
            cb.0
        } else {
            let pitch = pitch as usize;
            let (w2, h2) = (out.width as usize / 2, out.height as usize / 2);
            chroma.resize(pitch * h2, 128);
            interleave(&mut chroma, pitch, cb, cr, w2, h2);
            chroma.as_ptr()
        };
        il.add("pyrowave vo interleave", t.elapsed());
        let t = Instant::now();
        let r = unsafe { pf_vo_show(y, uv, pitch, out.width, out.height) };
        show.add("pyrowave vo displaypicture", t.elapsed());
        if r != 0 {
            tracing::warn!("pyrowave vo: DisplayPicture failed: {r}");
        }
        age.add("pyrowave decoded->shown", out.age(slot));
        if let Some(prev) = last.replace(Instant::now()) {
            interval.add("pyrowave vo interval", prev.elapsed().min(Duration::from_secs(1)));
        }
        drop(out);
        if !connected {
            vo_connect(&bus, width, height);
            connected = true;
        }
    }
    vo_detach(&bus);
}

pub fn current_output() -> Option<Arc<Output>> {
    OUTPUT.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

fn publish(output: Option<Arc<Output>>) {
    *OUTPUT.lock().unwrap_or_else(PoisonError::into_inner) = output;
}

fn icd_entry() -> Result<ash::Entry> {
    type Negotiate = unsafe extern "C" fn(*mut u32) -> i32;
    unsafe {
        let lib = libc::dlopen(c"libmali.so".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
        if lib.is_null() {
            bail!("dlopen libmali.so failed");
        }
        let negotiate = libc::dlsym(lib, c"vk_icdNegotiateLoaderICDInterfaceVersion".as_ptr());
        if !negotiate.is_null() {
            let mut version = 5u32;
            std::mem::transmute::<*mut c_void, Negotiate>(negotiate)(&mut version);
        }
        let gipa = libc::dlsym(lib, c"vk_icdGetInstanceProcAddr".as_ptr());
        if gipa.is_null() {
            bail!("libmali.so has no Vulkan ICD entry point");
        }
        Ok(ash::Entry::from_static_fn(ash::StaticFn {
            get_instance_proc_addr: std::mem::transmute::<*mut c_void, vk::PFN_vkGetInstanceProcAddr>(gipa),
        }))
    }
}

struct Probe {
    ok: bool,
    float16: bool,
    name: String,
}

fn probe_features(instance: &ash::Instance, pdev: vk::PhysicalDevice) -> Probe {
    let props = unsafe { instance.get_physical_device_properties(pdev) };
    let mut f12 = vk::PhysicalDeviceVulkan12Features::default();
    let mut f13 = vk::PhysicalDeviceVulkan13Features::default();
    let mut f2 = vk::PhysicalDeviceFeatures2::default()
        .push_next(&mut f12)
        .push_next(&mut f13);
    unsafe { instance.get_physical_device_features2(pdev, &mut f2) };
    let int16 = f2.features.shader_int16 == vk::TRUE;
    let ok = props.api_version >= vk::API_VERSION_1_3
        && int16
        && f12.storage_buffer8_bit_access == vk::TRUE
        && f12.timeline_semaphore == vk::TRUE
        && f13.subgroup_size_control == vk::TRUE
        && f13.compute_full_subgroups == vk::TRUE
        && f13.synchronization2 == vk::TRUE;
    Probe {
        ok,
        float16: f12.shader_float16 == vk::TRUE,
        name: props
            .device_name_as_c_str()
            .map(|c| c.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }
}

pub fn available() -> bool {
    static CAPABLE: OnceLock<bool> = OnceLock::new();
    *CAPABLE.get_or_init(|| match Gpu::new() {
        Ok(gpu) => {
            tracing::info!(
                "pyrowave: {} decodes PyroWave ({})",
                gpu.name,
                if gpu.linear {
                    "decodes into host-visible linear planes"
                } else {
                    "copies planes to a host buffer"
                }
            );
            true
        }
        Err(e) => {
            tracing::info!("pyrowave: unavailable ({e:#})");
            false
        }
    })
}

struct Hold {
    _app: Box<vk::ApplicationInfo<'static>>,
    instance_ci: Box<vk::InstanceCreateInfo<'static>>,
    _prio: Box<[f32; 1]>,
    _queue_ci: Box<[vk::DeviceQueueCreateInfo<'static>; 1]>,
    _f12: Box<vk::PhysicalDeviceVulkan12Features<'static>>,
    _f13: Box<vk::PhysicalDeviceVulkan13Features<'static>>,
    _f2: Box<vk::PhysicalDeviceFeatures2<'static>>,
    device_ci: Box<vk::DeviceCreateInfo<'static>>,
}

impl Hold {
    fn new(qf: u32, float16: bool) -> Self {
        let app = Box::new(vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_3));
        let mut instance_ci = Box::new(vk::InstanceCreateInfo::default());
        instance_ci.p_application_info = &*app;
        let prio = Box::new([1.0f32]);
        let mut queue_ci = Box::new([vk::DeviceQueueCreateInfo::default().queue_family_index(qf)]);
        queue_ci[0].queue_count = 1;
        queue_ci[0].p_queue_priorities = prio.as_ptr();
        let mut f13 = Box::new(
            vk::PhysicalDeviceVulkan13Features::default()
                .synchronization2(true)
                .subgroup_size_control(true)
                .compute_full_subgroups(true),
        );
        let mut f12 = Box::new(
            vk::PhysicalDeviceVulkan12Features::default()
                .timeline_semaphore(true)
                .storage_buffer8_bit_access(true)
                .shader_float16(float16),
        );
        f12.p_next = (&mut *f13 as *mut vk::PhysicalDeviceVulkan13Features).cast();
        let mut f2 = Box::new(vk::PhysicalDeviceFeatures2::default());
        f2.features.shader_int16 = vk::TRUE;
        f2.p_next = (&mut *f12 as *mut vk::PhysicalDeviceVulkan12Features).cast();
        let mut device_ci = Box::new(vk::DeviceCreateInfo::default());
        device_ci.p_next = (&*f2 as *const vk::PhysicalDeviceFeatures2).cast();
        device_ci.queue_create_info_count = 1;
        device_ci.p_queue_create_infos = queue_ci.as_ptr();
        Self {
            _app: app,
            instance_ci,
            _prio: prio,
            _queue_ci: queue_ci,
            _f12: f12,
            _f13: f13,
            _f2: f2,
            device_ci,
        }
    }
}

struct Gpu {
    entry: ash::Entry,
    instance: ash::Instance,
    pdev: vk::PhysicalDevice,
    device: ash::Device,
    queue: vk::Queue,
    qf: u32,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    ts_period_ns: Option<f64>,
    linear: bool,
    hold: Hold,
    name: String,
}

unsafe impl Send for Gpu {}

impl Gpu {
    fn new() -> Result<Self> {
        let entry = icd_entry()?;
        let pre = Hold::new(0, false);
        let instance =
            unsafe { entry.create_instance(&pre.instance_ci, None) }.map_err(|e| anyhow!("vkCreateInstance: {e}"))?;
        let found = (|| -> Result<_> {
            let pdevs = unsafe { instance.enumerate_physical_devices() }?;
            for pdev in pdevs {
                let probe = probe_features(&instance, pdev);
                if !probe.ok {
                    continue;
                }
                let families = unsafe { instance.get_physical_device_queue_family_properties(pdev) };
                if let Some(qf) = families
                    .iter()
                    .position(|f| f.queue_flags.contains(vk::QueueFlags::COMPUTE))
                {
                    return Ok((pdev, qf as u32, probe));
                }
            }
            bail!("no Vulkan 1.3 device with the PyroWave compute feature set")
        })();
        let (pdev, qf, probe) = match found {
            Ok(f) => f,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                return Err(e);
            }
        };
        let hold = Hold::new(qf, probe.float16);
        let device = match unsafe { instance.create_device(pdev, &hold.device_ci, None) } {
            Ok(d) => d,
            Err(e) => {
                unsafe { instance.destroy_instance(None) };
                bail!("vkCreateDevice: {e}");
            }
        };
        let queue = unsafe { device.get_device_queue(qf, 0) };
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pdev) };
        let ts_bits =
            unsafe { instance.get_physical_device_queue_family_properties(pdev) }[qf as usize].timestamp_valid_bits;
        let period = unsafe { instance.get_physical_device_properties(pdev) }
            .limits
            .timestamp_period;
        let ts_period_ns = (ts_bits > 0 && period > 0.0).then_some(f64::from(period));
        let r8 = unsafe { instance.get_physical_device_format_properties(pdev, vk::Format::R8_UNORM) };
        let linear = r8
            .linear_tiling_features
            .contains(vk::FormatFeatureFlags::STORAGE_IMAGE)
            && std::env::var_os("PF_PYRO_COPY").is_none();
        Ok(Self {
            entry,
            instance,
            pdev,
            device,
            queue,
            qf,
            mem_props,
            ts_period_ns,
            linear,
            hold,
            name: probe.name,
        })
    }

    fn memory_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count)
            .find(|&i| bits & (1 << i) != 0 && self.mem_props.memory_types[i as usize].property_flags.contains(want))
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
    }
}

struct Plane {
    width: u32,
    height: u32,
    image: vk::Image,
    memory: vk::DeviceMemory,
    mapped: *mut c_void,
    pitch: u32,
    coherent: bool,
    packed: bool,
}

impl Plane {
    fn texel_width(&self) -> u32 {
        if self.packed {
            self.width / 4
        } else {
            self.width
        }
    }
}

struct Slot {
    planes: Vec<Plane>,
    buffer: vk::Buffer,
    buffer_memory: vk::DeviceMemory,
    mapped: *mut c_void,
    coherent: bool,
    cmd: vk::CommandBuffer,
    fence: vk::Fence,
}

struct Decoder {
    stream: (u32, u32),
    pw_dev: pw::pyrowave_device,
    pw_dec: pw::pyrowave_decoder,
    pool: vk::CommandPool,
    queries: vk::QueryPool,
    slots: Vec<Slot>,
    output: Arc<Output>,
    done: Option<(mpsc::Sender<(usize, Instant, Instant, u64)>, std::thread::JoinHandle<()>)>,
}

struct Lane {
    gpu: Gpu,
    window: usize,
    dec: Option<Decoder>,
    frag: Vec<u8>,
    full_range: bool,
    record: Timing,
    cpu_dequant: Timing,
    slot_wait: Timing,
}

struct SlotSync {
    fence: vk::Fence,
    invalidate: Vec<vk::DeviceMemory>,
}

struct Completion {
    device: ash::Device,
    slots: Vec<SlotSync>,
    queries: vk::QueryPool,
    ts_period_ns: Option<f64>,
    output: Weak<Output>,
    vo: Option<mpsc::Sender<usize>>,
}

unsafe impl Send for Completion {}

impl Completion {
    fn run(self, pending: mpsc::Receiver<(usize, Instant, Instant, u64)>) {
        let mut wall = Timing::default();
        let mut gpu = Timing::default();
        let mut interval = Timing::default();
        let mut gpu_decode = Timing::default();
        let mut gpu_copy = Timing::default();
        let mut last: Option<Instant> = None;
        while let Ok((slot, started, submitted, pts_ns)) = pending.recv() {
            let sync = &self.slots[slot];
            let waited = unsafe {
                self.device.wait_for_fences(&[sync.fence], true, 1_000_000_000).and_then(|()| {
                    sync.invalidate.iter().try_for_each(|&memory| {
                        self.device.invalidate_mapped_memory_ranges(&[vk::MappedMemoryRange::default()
                            .memory(memory)
                            .size(vk::WHOLE_SIZE)])
                    })
                })
            };
            let Some(out) = self.output.upgrade() else {
                break;
            };
            if let Err(e) = waited {
                tracing::warn!("pyrowave decode: fence wait: {e}");
                out.busy.fetch_and(!(1 << slot), Ordering::AcqRel);
                continue;
            }
            let done = Instant::now();
            wall.add("pyrowave feed decode+copy wall", done - started);
            gpu.add("pyrowave submit->done", done - submitted);
            let queued = last.is_some_and(|prev| prev >= submitted);
            let own = done - last.map_or(submitted, |prev| prev.max(submitted));
            let old = out.gpu_ns.load(Ordering::Relaxed);
            let own = own.as_nanos() as u64;
            // A frame submitted to an idle GPU also counts submit and wake latency (~1 ms with the
            // CPU dequant ahead of it); only back-to-back frames measure the GPU.
            if queued || old == 0 {
                out.gpu_ns.store(if old == 0 { own } else { old - old / 8 + own / 8 }, Ordering::Relaxed);
            }
            if let Some(hud) = HUD.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
                hud.note_decoded(pts_ns, realtime_ns());
            }
            if let Some(prev) = last.replace(done) {
                interval.add("pyrowave decode interval", (done - prev).min(Duration::from_secs(1)));
            }
            if let Some((decode, copy)) = self.gpu_spans(slot) {
                gpu_decode.add("pyrowave gpu decode", decode);
                gpu_copy.add("pyrowave gpu copy", copy);
            }
            out.decoded_at[slot].store(now_ns(), Ordering::Release);
            out.latest.store(slot, Ordering::Release);
            out.seq.fetch_add(1, Ordering::AcqRel);
            out.busy.fetch_and(!(1 << slot), Ordering::AcqRel);
            if let Some(vo) = &self.vo {
                let _ = vo.send(slot);
            }
        }
    }

    fn gpu_spans(&self, slot: usize) -> Option<(Duration, Duration)> {
        let period = self.ts_period_ns.filter(|_| self.queries != vk::QueryPool::null())?;
        let mut ticks = [0u64; 3];
        let flags = vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WAIT;
        unsafe { self.device.get_query_pool_results(self.queries, (slot * 3) as u32, &mut ticks, flags) }.ok()?;
        let span = |a: u64, b: u64| Duration::from_nanos((b.saturating_sub(a) as f64 * period) as u64);
        Some((span(ticks[0], ticks[1]), span(ticks[1], ticks[2])))
    }
}

unsafe impl Send for Lane {}

fn pw_check(r: pw::pyrowave_result, what: &str) -> Result<()> {
    if r == pw::pyrowave_result_PYROWAVE_SUCCESS {
        Ok(())
    } else {
        bail!("pyrowave {what} failed: result {r}")
    }
}

fn seq_header_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    let w0 = u32::from_le_bytes(bytes.get(0..4)?.try_into().ok()?);
    let w1 = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    if w0 >> 31 == 0 || (w1 >> 24) & 0x3 != 0 {
        return None;
    }
    Some(((w0 & 0x3FFF) + 1, ((w0 >> 14) & 0x3FFF) + 1))
}

fn au_dims(au: &[u8], aligned: bool, window: usize) -> Option<(u32, u32)> {
    if !aligned {
        return seq_header_dims(au);
    }
    let win = &au[..au.len().min(window)];
    let used = usize::from(u16::from_le_bytes([*win.first()?, *win.get(1)?]));
    let kind = u16::from_le_bytes([*win.get(2)?, *win.get(3)?]);
    if used == 0 || 4 + used > win.len() || kind > 1 {
        return None;
    }
    seq_header_dims(&win[4..4 + used])
}

fn frame_bytes(width: u32, height: u32) -> u64 {
    u64::from(width) * u64::from(height) * 3 / 2
}

impl Lane {
    fn push_packet(&self, packet: &[u8]) -> Result<()> {
        let dec = self.dec.as_ref().context("no decoder")?;
        pw_check(
            unsafe { pw::pyrowave_decoder_push_packet(dec.pw_dec, packet.as_ptr().cast(), packet.len()) },
            "push_packet",
        )
    }

    fn push_window(&mut self, win: &[u8]) -> Result<()> {
        if win.len() < 4 {
            return Ok(());
        }
        let used = usize::from(u16::from_le_bytes([win[0], win[1]]));
        let kind = u16::from_le_bytes([win[2], win[3]]);
        if used == 0 || 4 + used > win.len() {
            self.frag.clear();
            return Ok(());
        }
        let body = &win[4..4 + used];
        match kind {
            0 => {
                self.frag.clear();
                self.push_packet(body)
            }
            1 => {
                self.frag.clear();
                self.frag.extend_from_slice(body);
                Ok(())
            }
            2 if !self.frag.is_empty() => {
                self.frag.extend_from_slice(body);
                Ok(())
            }
            3 if !self.frag.is_empty() => {
                self.frag.extend_from_slice(body);
                let packet = std::mem::take(&mut self.frag);
                let r = self.push_packet(&packet);
                self.frag = packet;
                self.frag.clear();
                r
            }
            _ => Ok(()),
        }
    }

    fn decode(&mut self, au: &[u8], aligned: bool, pts_ns: u64) -> Result<Option<Instant>> {
        if let Some(dims) = au_dims(au, aligned, self.window) {
            if self.dec.as_ref().is_none_or(|d| d.stream != dims) {
                self.reconfigure(dims.0, dims.1)?;
            }
        }
        if self.dec.is_none() {
            return Ok(None);
        }
        if aligned {
            self.frag.clear();
            for win in au.chunks(self.window) {
                self.push_window(win)?;
            }
        } else {
            self.push_packet(au)?;
        }
        let dec = self.dec.as_ref().context("no decoder")?;
        if !unsafe { pw::pyrowave_decoder_decode_is_ready(dec.pw_dec, true) } {
            return Ok(None);
        }
        let out = &dec.output;
        let wait_from = Instant::now();
        let deadline = wait_from + Duration::from_secs(1);
        let slot = loop {
            let busy = out.busy.load(Ordering::Acquire);
            let latest = out.latest.load(Ordering::Acquire);
            let shown = out.shown.load(Ordering::Acquire);
            let free = (0..RING).find(|s| busy & (1 << s) == 0 && *s != latest && *s != shown);
            match free {
                Some(s) if busy.count_ones() < IN_FLIGHT => break s,
                _ if Instant::now() > deadline => bail!("no free output slot"),
                // ponytail: polls while the GPU is the bottleneck; a condvar if this shows up in CPU time
                _ => std::thread::sleep(Duration::from_micros(200)),
            }
        };
        let started = Instant::now();
        self.slot_wait.add("pyrowave slot wait", started - wait_from);
        let submitted = self.record_and_submit(slot)?;
        self.record.add("pyrowave record+submit", submitted - started);
        if let Some(dec) = &self.dec {
            let us = unsafe { pw::pyrowave_decoder_cpu_dequant_us(dec.pw_dec) };
            if us > 0 {
                self.cpu_dequant.add("pyrowave cpu dequant", Duration::from_micros(us.into()));
            }
        }
        let dec = self.dec.as_ref().context("no decoder")?;
        if let Some((done, _)) = &dec.done {
            let _ = done.send((slot, started, submitted, pts_ns));
        }
        Ok(Some(submitted))
    }

    fn record_and_submit(&self, slot_index: usize) -> Result<Instant> {
        let dev = &self.gpu.device;
        let dec = self.dec.as_ref().context("no decoder")?;
        let slot = &dec.slots[slot_index];
        let timed = dec.queries != vk::QueryPool::null();
        let query = (slot_index * 3) as u32;
        let range = vk::ImageSubresourceRange {
            aspect_mask: vk::ImageAspectFlags::COLOR,
            base_mip_level: 0,
            level_count: 1,
            base_array_layer: 0,
            layer_count: 1,
        };
        let barrier = |p: &Plane,
                       src: (vk::PipelineStageFlags2, vk::AccessFlags2),
                       dst: (vk::PipelineStageFlags2, vk::AccessFlags2),
                       layouts: (vk::ImageLayout, vk::ImageLayout)| {
            vk::ImageMemoryBarrier2::default()
                .src_stage_mask(src.0)
                .src_access_mask(src.1)
                .dst_stage_mask(dst.0)
                .dst_access_mask(dst.1)
                .old_layout(layouts.0)
                .new_layout(layouts.1)
                .image(p.image)
                .subresource_range(range)
        };
        let compute_write = (
            vk::PipelineStageFlags2::COMPUTE_SHADER,
            vk::AccessFlags2::SHADER_STORAGE_WRITE,
        );
        let transfer_read = (vk::PipelineStageFlags2::COPY, vk::AccessFlags2::TRANSFER_READ);
        unsafe {
            dev.reset_fences(&[slot.fence])?;
            dev.begin_command_buffer(
                slot.cmd,
                &vk::CommandBufferBeginInfo::default().flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            if timed {
                dev.cmd_reset_query_pool(slot.cmd, dec.queries, query, 3);
            }
            let pre: Vec<_> = slot
                .planes
                .iter()
                .map(|p| {
                    barrier(
                        p,
                        (vk::PipelineStageFlags2::COPY, vk::AccessFlags2::NONE),
                        compute_write,
                        (vk::ImageLayout::UNDEFINED, vk::ImageLayout::GENERAL),
                    )
                })
                .collect();
            let previous = [vk::MemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COMPUTE_SHADER)
                .src_access_mask(vk::AccessFlags2::SHADER_STORAGE_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::COPY | vk::PipelineStageFlags2::COMPUTE_SHADER)
                .dst_access_mask(
                    vk::AccessFlags2::TRANSFER_WRITE
                        | vk::AccessFlags2::SHADER_STORAGE_READ
                        | vk::AccessFlags2::SHADER_STORAGE_WRITE,
                )];
            dev.cmd_pipeline_barrier2(
                slot.cmd,
                &vk::DependencyInfo::default()
                    .memory_barriers(&previous)
                    .image_memory_barriers(&pre),
            );
            if timed {
                dev.cmd_write_timestamp2(slot.cmd, vk::PipelineStageFlags2::ALL_COMMANDS, dec.queries, query);
            }
        }
        let view = |p: Option<&Plane>| {
            let Some(p) = p else {
                return unsafe { std::mem::zeroed::<pw::pyrowave_image_view>() };
            };
            let format = if p.packed {
                pw::VkFormat_VK_FORMAT_R32_UINT
            } else {
                pw::VkFormat_VK_FORMAT_R8_UNORM
            };
            pw::pyrowave_image_view {
                image: p.image.as_raw() as usize as pw::VkImage,
                width: p.texel_width(),
                height: p.height,
                image_format: format,
                view_format: format,
                mip_level: 0,
                layer: 0,
                aspect: pw::VkImageAspectFlagBits_VK_IMAGE_ASPECT_COLOR_BIT,
                swizzle: pw::VkComponentSwizzle_VK_COMPONENT_SWIZZLE_IDENTITY,
                layout: vk::ImageLayout::GENERAL.as_raw() as pw::VkImageLayout,
            }
        };
        let buffers = pw::pyrowave_gpu_buffers {
            planes: [view(slot.planes.first()), view(slot.planes.get(1)), view(slot.planes.get(2))],
        };
        let r = unsafe {
            pw::pyrowave_device_set_command_buffer(dec.pw_dev, slot.cmd.as_raw() as usize as pw::VkCommandBuffer);
            let r = pw::pyrowave_decoder_decode_gpu_buffer(dec.pw_dec, std::ptr::null(), std::ptr::null(), &buffers);
            pw::pyrowave_device_set_command_buffer(dec.pw_dev, std::ptr::null_mut());
            r
        };
        let linear = slot.buffer == vk::Buffer::null();
        unsafe {
            if timed {
                dev.cmd_write_timestamp2(slot.cmd, vk::PipelineStageFlags2::ALL_COMMANDS, dec.queries, query + 1);
            }
        }
        if linear {
            unsafe {
                let to_host: Vec<_> = slot
                    .planes
                    .iter()
                    .map(|p| {
                        barrier(
                            p,
                            compute_write,
                            (vk::PipelineStageFlags2::HOST, vk::AccessFlags2::HOST_READ),
                            (vk::ImageLayout::GENERAL, vk::ImageLayout::GENERAL),
                        )
                    })
                    .collect();
                dev.cmd_pipeline_barrier2(slot.cmd, &vk::DependencyInfo::default().image_memory_barriers(&to_host));
            }
        } else {
            self.record_copy(slot, &barrier, compute_write, transfer_read);
        }
        unsafe {
            if timed {
                dev.cmd_write_timestamp2(slot.cmd, vk::PipelineStageFlags2::ALL_COMMANDS, dec.queries, query + 2);
            }
            dev.end_command_buffer(slot.cmd)?;
            pw_check(r, "decode_gpu_buffer")?;
            let cmds = [slot.cmd];
            dec.output.busy.fetch_or(1 << slot_index, Ordering::AcqRel);
            if let Err(e) = dev.queue_submit(
                self.gpu.queue,
                &[vk::SubmitInfo::default().command_buffers(&cmds)],
                slot.fence,
            ) {
                dec.output.busy.fetch_and(!(1 << slot_index), Ordering::AcqRel);
                return Err(e.into());
            }
            Ok(Instant::now())
        }
    }

    fn record_copy<'a>(
        &self,
        slot: &Slot,
        barrier: &impl Fn(
            &Plane,
            (vk::PipelineStageFlags2, vk::AccessFlags2),
            (vk::PipelineStageFlags2, vk::AccessFlags2),
            (vk::ImageLayout, vk::ImageLayout),
        ) -> vk::ImageMemoryBarrier2<'a>,
        compute_write: (vk::PipelineStageFlags2, vk::AccessFlags2),
        transfer_read: (vk::PipelineStageFlags2, vk::AccessFlags2),
    ) {
        let dev = &self.gpu.device;
        unsafe {
            let mid: Vec<_> = slot
                .planes
                .iter()
                .map(|p| {
                    barrier(
                        p,
                        compute_write,
                        transfer_read,
                        (vk::ImageLayout::GENERAL, vk::ImageLayout::TRANSFER_SRC_OPTIMAL),
                    )
                })
                .collect();
            dev.cmd_pipeline_barrier2(slot.cmd, &vk::DependencyInfo::default().image_memory_barriers(&mid));
            let mut offset = 0u64;
            for p in &slot.planes {
                let region = vk::BufferImageCopy::default()
                    .buffer_offset(offset)
                    .image_subresource(vk::ImageSubresourceLayers {
                        aspect_mask: vk::ImageAspectFlags::COLOR,
                        mip_level: 0,
                        base_array_layer: 0,
                        layer_count: 1,
                    })
                    .image_extent(vk::Extent3D {
                        width: p.texel_width(),
                        height: p.height,
                        depth: 1,
                    });
                dev.cmd_copy_image_to_buffer(
                    slot.cmd,
                    p.image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    slot.buffer,
                    &[region],
                );
                offset += u64::from(p.width) * u64::from(p.height);
            }
            let to_host = [vk::BufferMemoryBarrier2::default()
                .src_stage_mask(vk::PipelineStageFlags2::COPY)
                .src_access_mask(vk::AccessFlags2::TRANSFER_WRITE)
                .dst_stage_mask(vk::PipelineStageFlags2::HOST)
                .dst_access_mask(vk::AccessFlags2::HOST_READ)
                .buffer(slot.buffer)
                .size(vk::WHOLE_SIZE)];
            dev.cmd_pipeline_barrier2(
                slot.cmd,
                &vk::DependencyInfo::default().buffer_memory_barriers(&to_host),
            );
        }
    }

    fn reconfigure(&mut self, width: u32, height: u32) -> Result<()> {
        if let Some(mut old) = self.dec.take() {
            self.destroy(&mut old);
        }
        if width % 2 != 0 || height % 2 != 0 {
            bail!("pyrowave 4:2:0 needs even dimensions (got {width}x{height})");
        }
        let mut dec = self.create(width, height)?;
        let mut vo = None;
        if dec.output.vo {
            let (tx, rx) = mpsc::channel();
            let weak = Arc::downgrade(&dec.output);
            std::thread::Builder::new()
                .name("pyrowave-vo".into())
                .spawn(move || vo_thread(weak, rx))?;
            vo = Some(tx);
        }
        let completion = Completion {
            device: self.gpu.device.clone(),
            slots: dec
                .slots
                .iter()
                .map(|s| SlotSync {
                    fence: s.fence,
                    invalidate: if s.buffer == vk::Buffer::null() {
                        s.planes.iter().filter(|p| !p.coherent).map(|p| p.memory).collect()
                    } else if !s.coherent {
                        vec![s.buffer_memory]
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
            queries: dec.queries,
            ts_period_ns: self.gpu.ts_period_ns,
            output: Arc::downgrade(&dec.output),
            vo,
        };
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pyrowave-done".into())
            .spawn(move || completion.run(rx))?;
        dec.done = Some((tx, thread));
        publish(Some(dec.output.clone()));
        tracing::info!(
            "pyrowave: decoder up at {width}x{height}, output {}x{}{}",
            dec.output.width,
            dec.output.height,
            if dec.output.vo { " on the video plane" } else { "" }
        );
        self.dec = Some(dec);
        Ok(())
    }

    fn create(&self, width: u32, height: u32) -> Result<Decoder> {
        let gpu = &self.gpu;
        let mut queue_info = pw::pyrowave_device_create_queue_info {
            queue: gpu.queue.as_raw() as usize as pw::VkQueue,
            familyIndex: gpu.qf,
            index: 0,
        };
        let create = pw::pyrowave_device_create_info {
            GetInstanceProcAddr: Some(unsafe {
                std::mem::transmute::<
                    vk::PFN_vkGetInstanceProcAddr,
                    unsafe extern "C" fn(pw::VkInstance, *const c_char) -> pw::PFN_vkVoidFunction,
                >(gpu.entry.static_fn().get_instance_proc_addr)
            }),
            instance: gpu.instance.handle().as_raw() as usize as pw::VkInstance,
            physical_device: gpu.pdev.as_raw() as usize as pw::VkPhysicalDevice,
            device: gpu.device.handle().as_raw() as usize as pw::VkDevice,
            instance_create_info: (&*gpu.hold.instance_ci as *const vk::InstanceCreateInfo).cast(),
            device_create_info: (&*gpu.hold.device_ci as *const vk::DeviceCreateInfo).cast(),
            queue_info: &mut queue_info,
            queue_info_count: 1,
            queue_lock_callback: None,
            queue_unlock_callback: None,
            userdata: std::ptr::null_mut(),
        };
        let mut pw_dev: pw::pyrowave_device = std::ptr::null_mut();
        pw_check(
            unsafe { pw::pyrowave_create_device(&create, &mut pw_dev) },
            "create_device",
        )?;
        unsafe { pw::pyrowave_device_set_queue_type(pw_dev, pw::VkQueueFlagBits_VK_QUEUE_COMPUTE_BIT) };
        let (dw, dh) = *DISPLAY.get().unwrap_or(&(1920, 1080));
        let vo = vo_wanted(width, height);
        let half = !vo && (width > dw || height > dh) && width % 4 == 0 && height % 4 == 0;
        let nv12 = vo && width % 8 == 0 && app_flag("pyro-gpu-nv12");
        std::env::set_var("PYROWAVE_HALF_OUTPUT", if half { "1" } else { "0" });
        // Above 1080p the GPU dequant is what misses 60 fps; the CPU does it on spare cores.
        if std::env::var_os("PYROWAVE_CPU_DEQUANT").is_none() {
            std::env::set_var("PYROWAVE_CPU_DEQUANT", if width * height > 1920 * 1080 { "1" } else { "0" });
        }
        let (out_w, out_h) = if half { (width / 2, height / 2) } else { (width, height) };
        let dinfo = pw::pyrowave_decoder_create_info {
            device: pw_dev,
            width: width as i32,
            height: height as i32,
            chroma: pw::pyrowave_chroma_subsampling_PYROWAVE_CHROMA_SUBSAMPLING_420,
            fragment_path: false,
        };
        let mut pw_dec: pw::pyrowave_decoder = std::ptr::null_mut();
        if let Err(e) = pw_check(
            unsafe { pw::pyrowave_decoder_create(&dinfo, &mut pw_dec) },
            "decoder_create",
        ) {
            unsafe { pw::pyrowave_device_destroy(pw_dev) };
            return Err(e);
        }
        let pool = unsafe {
            gpu.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(gpu.qf)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        let queries = match gpu.ts_period_ns {
            Some(_) => unsafe {
                gpu.device.create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count((RING * 3) as u32),
                    None,
                )
            }
            .unwrap_or(vk::QueryPool::null()),
            None => vk::QueryPool::null(),
        };
        let mut dec = Decoder {
            stream: (width, height),
            pw_dev,
            pw_dec,
            pool,
            queries,
            slots: Vec::new(),
            done: None,
            output: Arc::new(Output {
                width: out_w,
                height: out_h,
                generation: GENERATION.fetch_add(1, Ordering::Relaxed) + 1,
                full_range: self.full_range,
                vo,
                nv12,
                decoded_at: Default::default(),
                frames: [[(0, 0); 3]; RING],
                latest: AtomicUsize::new(NO_SLOT),
                seq: AtomicU64::new(0),
                shown: AtomicUsize::new(NO_SLOT),
                busy: AtomicUsize::new(0),
                gpu_ns: AtomicU64::new(0),
            }),
        };
        let mut frames = [[(0usize, 0u32); 3]; RING];
        for (i, frame) in frames.iter_mut().enumerate() {
            match self.make_slot(pool, out_w, out_h, nv12) {
                Ok(slot) => {
                    let mut offset = 0usize;
                    for (view, p) in frame.iter_mut().zip(&slot.planes) {
                        *view = if slot.buffer == vk::Buffer::null() {
                            (p.mapped as usize, p.pitch)
                        } else {
                            (slot.mapped as usize + offset, p.width)
                        };
                        offset += (p.width * p.height) as usize;
                    }
                    dec.slots.push(slot);
                }
                Err(e) => {
                    self.destroy(&mut dec);
                    return Err(e.context(format!("output slot {i}")));
                }
            }
        }
        if let Some(out) = Arc::get_mut(&mut dec.output) {
            out.frames = frames;
        }
        Ok(dec)
    }

    fn make_slot(&self, pool: vk::CommandPool, width: u32, height: u32, nv12: bool) -> Result<Slot> {
        let gpu = &self.gpu;
        let dev = &gpu.device;
        let mut slot = Slot {
            planes: Vec::new(),
            buffer: vk::Buffer::null(),
            buffer_memory: vk::DeviceMemory::null(),
            mapped: std::ptr::null_mut(),
            coherent: false,
            cmd: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
        };
        let built = (|| -> Result<()> {
            let planes: &[(u32, u32)] = if nv12 {
                &[(width, height), (width, height / 2)]
            } else {
                &[(width, height), (width / 2, height / 2), (width / 2, height / 2)]
            };
            for &(w, h) in planes {
                slot.planes.push(self.make_plane(w, h)?);
            }
            unsafe {
                slot.cmd = dev.allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )?[0];
                slot.fence = dev.create_fence(&vk::FenceCreateInfo::default(), None)?;
                if gpu.linear {
                    return Ok(());
                }
                slot.buffer = dev.create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(frame_bytes(width, height))
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    None,
                )?;
                let req = dev.get_buffer_memory_requirements(slot.buffer);
                let cached = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
                let type_index = gpu
                    .memory_type(req.memory_type_bits, cached)
                    .or_else(|| gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE))
                    .context("no host-visible memory for the frame buffer")?;
                slot.coherent = gpu.mem_props.memory_types[type_index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_COHERENT);
                slot.buffer_memory = dev.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(type_index),
                    None,
                )?;
                dev.bind_buffer_memory(slot.buffer, slot.buffer_memory, 0)?;
                slot.mapped = dev.map_memory(slot.buffer_memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
            }
            Ok(())
        })();
        if let Err(e) = built {
            self.destroy_slot(&slot);
            return Err(e);
        }
        Ok(slot)
    }

    fn make_plane(&self, width: u32, height: u32) -> Result<Plane> {
        let gpu = &self.gpu;
        let mut plane = Plane {
            width,
            height,
            image: vk::Image::null(),
            memory: vk::DeviceMemory::null(),
            mapped: std::ptr::null_mut(),
            pitch: width,
            coherent: true,
            packed: width % 4 == 0,
        };
        let (tiling, usage) = if gpu.linear {
            (vk::ImageTiling::LINEAR, vk::ImageUsageFlags::STORAGE)
        } else {
            (
                vk::ImageTiling::OPTIMAL,
                vk::ImageUsageFlags::STORAGE | vk::ImageUsageFlags::TRANSFER_SRC,
            )
        };
        let made = (|| -> Result<()> {
            unsafe {
                plane.image = gpu.device.create_image(
                    &vk::ImageCreateInfo::default()
                        .image_type(vk::ImageType::TYPE_2D)
                        .format(if plane.packed { vk::Format::R32_UINT } else { vk::Format::R8_UNORM })
                        .extent(vk::Extent3D {
                            width: plane.texel_width(),
                            height,
                            depth: 1,
                        })
                        .mip_levels(1)
                        .array_layers(1)
                        .samples(vk::SampleCountFlags::TYPE_1)
                        .tiling(tiling)
                        .usage(usage)
                        .initial_layout(vk::ImageLayout::UNDEFINED),
                    None,
                )?;
                let req = gpu.device.get_image_memory_requirements(plane.image);
                let cached = vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED;
                let type_index = if gpu.linear {
                    gpu.memory_type(req.memory_type_bits, cached)
                        .or_else(|| gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE))
                } else {
                    gpu.memory_type(req.memory_type_bits, vk::MemoryPropertyFlags::DEVICE_LOCAL)
                }
                .context("no memory type for a plane")?;
                plane.coherent = gpu.mem_props.memory_types[type_index as usize]
                    .property_flags
                    .contains(vk::MemoryPropertyFlags::HOST_COHERENT);
                plane.memory = gpu.device.allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(req.size)
                        .memory_type_index(type_index),
                    None,
                )?;
                gpu.device.bind_image_memory(plane.image, plane.memory, 0)?;
                if gpu.linear {
                    let layout = gpu.device.get_image_subresource_layout(
                        plane.image,
                        vk::ImageSubresource {
                            aspect_mask: vk::ImageAspectFlags::COLOR,
                            mip_level: 0,
                            array_layer: 0,
                        },
                    );
                    let base = gpu
                        .device
                        .map_memory(plane.memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?;
                    plane.mapped = base.cast::<u8>().add(layout.offset as usize).cast();
                    plane.pitch = u32::try_from(layout.row_pitch)?;
                }
            }
            Ok(())
        })();
        if let Err(e) = made {
            self.destroy_plane(&plane);
            return Err(e);
        }
        Ok(plane)
    }

    fn destroy_plane(&self, plane: &Plane) {
        unsafe {
            self.gpu.device.destroy_image(plane.image, None);
            self.gpu.device.free_memory(plane.memory, None);
        }
    }

    fn destroy_slot(&self, slot: &Slot) {
        for p in &slot.planes {
            self.destroy_plane(p);
        }
        unsafe {
            self.gpu.device.destroy_buffer(slot.buffer, None);
            self.gpu.device.free_memory(slot.buffer_memory, None);
            self.gpu.device.destroy_fence(slot.fence, None);
        }
    }

    fn destroy(&self, dec: &mut Decoder) {
        publish(None);
        if let Some((done, thread)) = dec.done.take() {
            drop(done);
            let _ = thread.join();
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(&dec.output) > 1 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        unsafe {
            let _ = self.gpu.device.queue_wait_idle(self.gpu.queue);
        }
        for slot in &dec.slots {
            self.destroy_slot(slot);
        }
        unsafe {
            self.gpu.device.destroy_command_pool(dec.pool, None);
            self.gpu.device.destroy_query_pool(dec.queries, None);
            pw::pyrowave_decoder_destroy(dec.pw_dec);
            pw::pyrowave_device_destroy(dec.pw_dev);
        }
    }
}

impl Drop for Lane {
    fn drop(&mut self) {
        if let Some(mut dec) = self.dec.take() {
            self.destroy(&mut dec);
        }
    }
}

struct Pending {
    au: Vec<u8>,
    aligned: bool,
    pts_ns: u64,
    fed_at: Instant,
}

#[derive(Default)]
struct Shared {
    next: Mutex<Option<Pending>>,
    ready: Condvar,
    dead: AtomicBool,
    quit: AtomicBool,
}

impl Shared {
    fn take(&self, until: Option<Instant>) -> Option<Pending> {
        let mut next = self.next.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.quit.load(Ordering::Acquire) {
                return None;
            }
            let now = Instant::now();
            match until {
                Some(t) if now < t => {
                    next = self.ready.wait_timeout(next, t - now).unwrap_or_else(PoisonError::into_inner).0;
                }
                _ if next.is_some() => return next.take(),
                _ => next = self.ready.wait(next).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }
}

fn margin() -> Duration {
    static MARGIN: OnceLock<Duration> = OnceLock::new();
    *MARGIN.get_or_init(|| {
        let us = std::env::var("PF_PYRO_MARGIN_US").ok().and_then(|v| v.parse().ok()).unwrap_or(1500);
        Duration::from_micros(us)
    })
}

fn decode_thread(mut lane: Lane, shared: Arc<Shared>) {
    if let Some(n) = std::env::var("PYROWAVE_CPU_DEQUANT_NICE").ok().and_then(|n| n.parse::<i32>().ok()) {
        let r = unsafe { libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, n) };
        tracing::info!("pyrowave decode thread nice {n}: {r}");
    }
    let mut record = Duration::from_millis(3);
    let mut due: Option<Instant> = None;
    let (mut queue, mut timer, mut notify) = (Timing::default(), Timing::default(), Timing::default());
    let mut census = Census::new();
    loop {
        let until = due.and_then(|d| d.checked_sub(record + margin()));
        let Some(p) = shared.take(until) else {
            break;
        };
        let started = Instant::now();
        let queued = started.saturating_duration_since(p.fed_at);
        queue.add("pyrowave feed queue", queued);
        match until.filter(|t| p.fed_at <= *t) {
            Some(t) => timer.add("pyrowave decode timer overshoot", started.saturating_duration_since(t)),
            None => notify.add("pyrowave decode notify wake", queued),
        }
        census.tick();
        match lane.decode(&p.au, p.aligned, p.pts_ns) {
            Ok(Some(submitted)) => {
                record = (record * 7 + (submitted - started)) / 8;
                let gpu = lane
                    .dec
                    .as_ref()
                    .map_or(0, |d| d.output.gpu_ns.load(Ordering::Relaxed));
                let start = due.filter(|d| *d > submitted).unwrap_or(submitted);
                due = Some(start + Duration::from_nanos(gpu));
            }
            Ok(None) => {}
            Err(e) => {
                if lane.dec.is_none() {
                    shared.dead.store(true, Ordering::Release);
                }
                tracing::warn!("pyrowave decode: {e:#}");
            }
        }
        if due.is_some_and(|d| d <= Instant::now()) {
            due = None;
        }
    }
}

pub struct PyroSink {
    shared: Arc<Shared>,
    rx: Mutex<RxTimes>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl PyroSink {
    pub fn new(shard_payload: usize, full_range: bool) -> Result<Self> {
        let lane = Lane {
            gpu: Gpu::new()?,
            window: shard_payload.max(1),
            dec: None,
            frag: Vec::new(),
            full_range,
            record: Timing::default(),
            cpu_dequant: Timing::default(),
            slot_wait: Timing::default(),
        };
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("pyrowave-decode".into())
            .spawn(move || decode_thread(lane, worker))?;
        Ok(Self {
            shared,
            rx: Mutex::default(),
            thread: Some(thread),
        })
    }
}

impl Drop for PyroSink {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        self.shared.ready.notify_all();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl VideoSink for PyroSink {
    fn name(&self) -> &'static str {
        "PyroWave (Vulkan)"
    }

    fn caps(&self) -> VideoSinkCaps {
        VideoSinkCaps::FEED_ONLY
    }

    fn feed(&self, au: &[u8], pts_ns: u64) -> Result<()> {
        self.feed_flags(au, pts_ns, 0)
    }

    fn feed_flags(&self, au: &[u8], pts_ns: u64, user_flags: u32) -> Result<()> {
        if self.is_dead() {
            bail!("pyrowave decoder is dead");
        }
        let aligned = user_flags & punktfunk_core::packet::USER_FLAG_CHUNK_ALIGNED != 0;
        let (first, last, now) = (ARRIVAL[0].load(Ordering::Relaxed), ARRIVAL[1].load(Ordering::Relaxed), realtime_ns());
        {
            let mut rx = self.rx.lock().unwrap_or_else(PoisonError::into_inner);
            if first != 0 && last >= first {
                rx.assembly.add("pyrowave rx assembly", Duration::from_nanos(last - first));
            }
            if last != 0 && now >= last {
                rx.handoff.add("pyrowave rx->feed", Duration::from_nanos(now - last));
            }
        }
        FED.fetch_add(1, Ordering::Relaxed);
        let mut next = self.shared.next.lock().unwrap_or_else(PoisonError::into_inner);
        if next.is_some() {
            REPLACED.fetch_add(1, Ordering::Relaxed);
        }
        let mut p = next.take().unwrap_or(Pending {
            au: Vec::new(),
            aligned,
            pts_ns,
            fed_at: Instant::now(),
        });
        p.fed_at = Instant::now();
        p.au.clear();
        p.au.extend_from_slice(au);
        p.aligned = aligned;
        p.pts_ns = pts_ns;
        *next = Some(p);
        drop(next);
        self.shared.ready.notify_one();
        Ok(())
    }

    fn is_dead(&self) -> bool {
        self.shared.dead.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn take_waits_for_the_deadline_and_returns_the_newest() {
        let pending = |n: u8| Pending {
            au: vec![n],
            aligned: false,
            pts_ns: u64::from(n),
            fed_at: Instant::now(),
        };
        let shared = Arc::new(Shared::default());
        *shared.next.lock().unwrap() = Some(pending(1));
        let late = shared.clone();
        let feeder = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            *late.next.lock().unwrap() = Some(pending(2));
            late.ready.notify_one();
        });
        let start = Instant::now();
        let got = shared.take(Some(start + Duration::from_millis(40))).unwrap();
        feeder.join().unwrap();
        assert!(start.elapsed() >= Duration::from_millis(40));
        assert_eq!(got.au, vec![2]);
        *shared.next.lock().unwrap() = Some(pending(3));
        let start = Instant::now();
        assert_eq!(shared.take(Some(start)).unwrap().au, vec![3]);
        assert!(start.elapsed() < Duration::from_millis(20));
    }

    #[test]
    fn sequence_header_dims() {
        let w0: u32 = (1 << 31) | ((1080 - 1) << 14) | (1920 - 1);
        let mut header = w0.to_le_bytes().to_vec();
        header.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(seq_header_dims(&header), Some((1920, 1080)));
        let mut window = vec![8u8, 0, 0, 0];
        window.extend_from_slice(&header);
        window.resize(64, 0);
        assert_eq!(au_dims(&window, true, 64), Some((1920, 1080)));
        window[2] = 2;
        assert_eq!(au_dims(&window, true, 64), None);
        assert_eq!(seq_header_dims(&[0; 8]), None);
        assert_eq!(frame_bytes(1920, 1080), 3_110_400);
    }
}
