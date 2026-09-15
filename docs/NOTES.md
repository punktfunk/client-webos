# Architecture & platform gotchas

Verified against LG CX (webOS 5.6) and G5 (webOS 10.3). Load-bearing decisions only.

## Toolchain

- Cross target `armv7-unknown-linux-gnueabi` (tier-2) + webosbrew toolchain. Linux-aarch64-only; CI native, dev Docker.
- `.cargo/config.toml` wires linker to `scripts/cc-shim.sh` (passes `--sysroot` explicitly).
- **Soft-float was single biggest perf fix** (~300ms → ~30ms per render). Non-`hf` target spec disables hardware FP codegen despite VFP3/NEON existing. Fix: `target-feature=+neon,+vfp3,-soft-float` + `target-cpu=cortex-a73` in `.cargo/config.toml`. Changes *codegen* only, not FFI ABI.
- **glibc shims required** (`src/platform/webos/glibc_compat_shim.c`): webOS glibc ~2.12 predates `getauxval`/`gettid`/`sendmmsg`. Linked via `cargo:rustc-link-arg`, **must land AFTER libstd** (single-pass linker drops `link-lib=static` too early).
- **SDL2 must be webosbrew fork** (release-2.30.12-webos.5, not generic SDL2). Only fork has Wayland shell-integration (`QT_WAYLAND_SHELL_INTEGRATION=webos`). On-device system copy is 2.0.10 (too old). Bundle own libSDL2 with `$ORIGIN/../lib` RPATH (set in `build.rs`).
- **cmake/opus**: `punktfunk-core`'s `quic` feature needs CMAKE_POLICY_VERSION_MINIMUM=3.5 (modern CMake refuses vendored libopus's old minimum).
- **Release builds are fat LTO, one codegen unit** (`Taskfile.yml`/`taskfiles/toolchain.yml`
  `RELEASE_LTO`). `Cargo.toml`'s profile has said so all along, but the task default was `thin` with
  16 units, which is what every `docker:build`, `docker:package`, `deploy` and CI package actually
  shipped — so the cross-crate inlining the hot loops were written for (AEAD decrypt, FEC, QUIC
  parsing, all in `punktfunk-core`'s dependencies) was never in the binary on the one target whose
  CPU cannot absorb the difference. `RELEASE_LTO=thin` is still there for a faster local cycle.
- **libstdc++ is linked statically, never bundled.** A bundled `lib/libstdc++.so.6` is found
  through the binary's `DT_RPATH`, which outranks the jail's `LD_LIBRARY_PATH`, so every library
  the process loads gets the SDK's copy — including the TV's own. webOS 11's
  `libNDL_media_impl.so.1` wants `GLIBCXX_3.4.32`, which that copy lacks, and the app exits at the
  splash with "Failed to load webOS libraries". Dropping the bundle outright breaks webOS 10 and
  below (their 6.0.29 lacks `GLIBCXX_3.4.30`). Static is the only one artifact correct on every
  firmware, and nothing C++ crosses the boundary — SDL, NDL and Luna are all C.

## UI preview (container)

`task docker:deploy` runs the app in a container on a virtual 1080p display and serves it over
VNC (`http://localhost:6080/vnc.html`), so UI work needs no TV. Same image, mounts and cache
volumes as the cross-build tasks.

- **Build with the `preview` profile** (release codegen, no LTO). A `dev` build of Skia's callers
  reads as input lag on top of llvmpipe and the VNC round trip. `PROFILE=dev` if rebuild time
  matters more.
- **No GPU and no mDNS.** llvmpipe means animation timing here is not the TV's, and Docker's "host"
  is the Linux VM, so `services::discovery` sees no LAN multicast — add hosts by hand. Unicast is
  unaffected, so a hand-entered host pairs and speed-tests for real.
- **Launch params reach the app as the argv[1] JSON SAM sends on a TV**, so `WEBOS_SDK=4.0.0`
  exercises the NDL v1 path here too. Telemetry lands in the terminal at `TELEMETRY_LEVEL`.

## UI rendering

Immediate mode on Skia over the shell's GL context (`console::gl`), drawn with the console kit
(`pf_console_ui`) — menus, in-stream overlays and the gamepad shell alike. Redraw on change and
while anything animates; the stream loop keeps its own 33 ms / 500 ms cadence for the overlays and
clears transparent so NDL's plane shows through.

- **Covers are Skia images built from the art loader's RGBA buffers** (`app::draw::home`), one copy
  when the art lands; Skia uploads on first draw and keeps the texture. The window that requests
  and evicts them is `app::render::prepare_grid`.
- **Text is the kit's**: Geist through Skia, shaped per frame. A string drawn every frame is fine;
  a document (About) is wrapped once per width and kept.
- **Glass over the menu is a backdrop blur** (`app::draw::glass_card`); over the stream there is no
  framebuffer to blur, so the dialog is the kit's panel on a transparent clear.
- **Modal and game-card glass share the face, rim shader and GPU blur path.** The game strip
  samples its cover; dialogs sample the page. Warmup uses the real game-strip painter and
  advances separate host/settings widgets through arrival and settling, submitting each sample.
  Warming only the material or a few static samples misses programs used later in the animation.
  Driver measurements confirmed 19–36 ms compilation stalls during first openings; the complete
  warmup removed the reported stutter on TV. Keep it once per GL context. Release the single
  cached cover blur before streaming, while preserving compiled programs.
- **Keep list widgets alive through modal fades.** Recreating the departing list restarts its
  row entrance while the panel closes. Cross-fades need separate widgets for both screens;
  sharing one slot rebuilds each on every frame. Retire the outgoing widget after its fade.
- **The drawable can differ from the display mode** on webOS: every frame scales the canvas from
  `display_mode` units to `drawable_size`, and every layout and hit test works in display units.
- **Icons are Lucide, by name** (`app::view::icons`), from the kit's table. A new mark is added to
  `assets/lucide/` in `unom/punktfunk` and regenerated there, not here.
- **Slow frames are GPU raster inside the shared shell, not this client's painters** (measured on
  the TV). The one client-controlled cost worth fixing was cover art: covers are normalised on the
  way into the cache (≤480x720, re-encoded **JPEG**, older entries shrunk in place on first read)
  because the shell's scaled-decode fast path only fires on JPEG, and decode happens on the fetch
  thread, not the render thread. Per-card antialiased clips and per-frame mask-filter blurs were
  the other costs — draw covers as one rounded rect with an image shader, bake halos and shadows as
  nine-patches.
- **The menu is scaled to the panel's physical size.** webOS hands every set the same 1920x1080
  surface, so text on a 48-inch panel is physically smaller by the diagonal ratio, and there is no
  display-scale signal to follow — the diagonal itself is the signal
  (`tv.model.moduleInchType` in LG's per-model config). The correction is the **square root** of
  the ratio (a smaller set is usually a closer set), capped at 1.25, and it divides the layout box
  while the canvas scale makes it back up, which is what carries Home's pixel geometry along with
  the type. Home's 1080p pixel metrics must go through `px_1080`, or the division cancels the canvas
  scale and boxes grow while the type inside them doesn't. **The gamepad shell opts out**: the kit's
  design box is `height / 800` and its screens are laid out to fill it, so a correction shortens the
  box and the screens run off the bottom instead of reflowing.
- ⚠ **The `ui_scale` launch param is deliberately untyped.** It is a launch param rather than a
  settings row so an unowned panel can be tuned on glass — and a field that rejects the string
  `ares-launch` sends fails the whole struct, silently costing every other param.

## Video decode (NDL DirectMedia)

- `libNDL_directmedia.so.1` is the real device library; the NDK sysroot ships a link-time stub.
- PTS = milliseconds since `NDL_DirectMediaLoad`, not wall-clock.
- Audio is decoded client-side via Opus unless offload is on — see *NDL's audio plane*.
- **`core::caps` has three readers that must agree**: `session::connect` (source of truth,
  advertised on the wire), `ui::settings` (what's offerable) and `Settings::clamp_to_caps`. A
  backend that changes the limits changes all three.
- **Decouple decode dimensions from punch-through rect** — else a 1080p stream on a 4K panel
  punches only the top-left quarter.
- **Loss recovery required** — no periodic IDRs in the stream. `session::pump::video_pump` calls
  `note_frame_index()` every frame (throttled RFI on gaps) plus a `request_keyframe()` backstop
  when `frames_dropped()` climbs.
- **Freeze-until-reanchor adapted for NDL**: NDL does decode+present in one opaque call (no split),
  so the client reimplements the skip-until-reanchor subset. A forward gap arms `holding`; frames
  are withheld until one arrives with `FLAG_SOF` (IDR) or a recovery anchor.
- ⚠ **Take the keyframe-request slot only while frames are still SKIPPED, never before asking
  whether this frame lifts the hold.** The resume frame restarts decoding on its own. Asking first
  returned `NeedKeyframe` for a frame that was in fact fed, and `submit` reads every non-`Presented`
  result as "this AU cannot complete" — so on a slice-progressive session (every NDL v2 stream) it
  dropped the remaining pieces of the very keyframe that resumed the picture and marked the next AU
  lost: a second freeze immediately after recovery, waiting out the host's 750 ms IDR cooldown.
- **Count a picture on `Presented`, not on arrival** — otherwise the overlay's fps ticks through a
  freeze and a play the decoder refused counts as a frame.
- **Name a hold only once it outlasts a blip** (`HOLD_TOAST_AFTER`, 300 ms, once per hold). An RFI
  recovery lifts one inside a round trip and the startup capacity probe's own loss clears at the
  burst's end, so a rising-edge toast fired at the start of every Wi-Fi session.
- HDR mastering metadata can change mid-session — drain `next_hdr_meta` every frame.
- **`NDL_DirectVideoSetHDRInfo` forces the panel into HDR mode on *any* call** (OLED65CX, webOS 5):
  it ignores an SDR `transfer`/`primaries` triplet and emits an HDR infoframe regardless, so an
  SDR/H.264 stream showed in HDR picture mode. `ndl::v2::set_color_info` therefore no-ops when
  `meta` is `None` (SDR) — only genuine HDR mastering metadata reaches NDL. Cost: NDL can no longer
  fix a bitstream's missing VUI colour info. HDR is also gated to HEVC end-to-end
  (`session::connect`: `apply_hdr = host_hdr && codec==H265`).
- **Re-applying HDR info per packet drops the panel to 60 Hz.** Re-entering HDR mode on every host
  HDR packet is what made 1440p120+ HDR stutter; apply once and on change only.

## DualSense feedback: hidraw when wired, the Bluetooth service otherwise

**A wired pad gets `/dev/hidraw0`**, `root:jailer` and read-write — the group the app runs in
(verified from inside the app on a G5, webOS 10.3, non-rooted). The earlier "the jail exposes no
hidraw at all" was probed with no pad plugged in, and `hid-playstation` only creates a node for a
device that exists. There is no `/sys/class/hidraw` in the jail, so the node is identified by
asking it (`HIDIOCGRAWINFO`), not by walking sysfs. A wired pad takes the 48-byte `0x02` report
with **no CRC** and no throttle — one syscall per write — carrying the same 47-byte common block
the Bluetooth `0x31` does, which is why every effect works on both. A Bluetooth pad also has a
hidraw node, but its reports need the `0x31` framing and CRC, so `hidraw.rs` claims `BUS_USB` only
and Bluetooth stays on the Luna route.

Adaptive triggers work on a **non-rooted** TV, but not through SDL. Verified end-to-end on G5
(dev-mode install, `DualSense` over Bluetooth): trigger resistance, section walls and lightbar
colour all confirmed on real hardware.

- **The Bluetooth route is** `luna://com.webos.service.bluetooth2/hid/internal/sendData`, which
  writes an arbitrary HID output report to the pad. Permitted because `compat.api.json` places it
  in the **`public`** API group, and `/usr/share/luna-service2/devmode_certificate.json` grants a
  dev-mode app `["ares.webos.cli", "public"]`. The restricted `devices`/`bluetooth.manage` groups
  are not needed.
- **Payload traps** (each cost hours): `reportData` must be an int array **with no `reportId` key**
  — one extra property fails the whole call with a generic "does not match the expected schema"
  naming nothing. `setReport` never works (always error 4); only `sendData` does. `getReport`
  *hangs* on a pad that doesn't answer, so callers need a deadline.
- **The report must be CRC-signed** exactly as the kernel's `hid-playstation`: 78 bytes (`0x31`,
  seq<<4, `0x10` tag, 47-byte common block, 24 reserved, CRC32-LE), CRC over the `0xA2` seed byte
  plus the report body. **A wrong CRC is silently ignored by the pad while the service still
  answers `returnValue: true`** — the most misleading failure mode here. Don't prepend `0xA2` to
  `reportData`; the stack adds the HIDP header itself.
- LG backported `hid-playstation` to kernel 5.4, so the pad binds as three input devices
  (pad/motion/touchpad) sharing one `U: Uniq=` MAC — where `dualsense::find_address` reads it.
- **Rumble does not use either path**: the pad's event node advertises `EV_FF` and is
  group-writable by `compositor` (the app's uid is in it), so rumble goes through SDL's evdev force
  feedback and works for any pad. Reports in `dualsense.rs` deliberately never set the
  compatible-vibration valid flag, so the paths can't fight.
- `hid/internal/*` is undocumented vendor surface — feature-detected, failing soft, never assumed.
- **Spawned sends must be throttled, or the video plane goes black.** The fallback route
  forks/execs `luna-send-pub` per send, copying the page tables of a process holding SDL, the
  decoder and its buffers. A Steam/Gamescope host *animates* the lightbar, so an unthrottled
  spawner produced a **black panel with the frame counter climbing, `dropped=0`, `backlog=0`**:
  decode kept running while the compositor never presented. `dualsense.rs` drops identical states
  and spaces the rest by `MIN_SEND_INTERVAL` (250 ms) on that route.
- **In-process LS2 works, from the app's binary path only** (`platform::webos::ls2`, verified on
  G5): `LSRegister(<app id>)` succeeds when the calling executable is
  `…/applications/<appid>/bin/punktfunk-webos` and `bluetooth2` methods reply in 1–3 ms; the
  identical binary anywhere else gets `Invalid permissions` — the hub keys permissions on the exe
  path. `LSRegisterApplicationService` is refused even there. `libluna-service2.so.3` +
  `libglib-2.0.so.0` are dlopened, so a refusal degrades to the spawn route (16 ms vs 250 ms).
- **Seeing Luna replies from the dev-mode ssh jail is impossible**: no `/dev/ptmx` (so
  `script`/`ssh -tt` fail) and `luna-send-pub` writes nothing without a tty, even to a file. Probe
  from inside the app, or with a small LS2 binary *copied over the app's own binary* (`rm` + `cp` —
  the installed file is root-owned 755 but its dir is group-writable). That trick also installs a
  package when `ares-install` fails on `rm -rf /media/developer/temp` (root-owned): call
  `com.webos.appInstallService/dev/install` with
  `{"id":"com.ares.defaultName","ipkUrl":"/media/developer/temp/x.ipk","subscribe":true}` from the
  app identity.

Host side: a game only emits trigger effects when it sees a `DualSense`, so the pad kind in the
handshake decides whether this feature does anything. Settings' **Controller** row
(`store::GamepadType`) defaults to `Automatic`, which **mirrors the attached pad**
(`gamepad::detect_type`) rather than sending wire `GamepadPref::Auto` — that wire value means "host
decides", and the host decides Xbox 360, which is why a `DualSense` first showed as an Xbox pad
with no effects. Resolution happens per session (`runtime::resolve_gamepad_type`) and deliberately
doesn't write back, so the stored preference keeps meaning "match my pad". Host env
`PUNKTFUNK_TEST_FEEDBACK` makes the host send a scripted lightbar/LED/trigger burst — use it to
test without a game.

## DualSense audio over Bluetooth: sniff mode is the whole problem

The pad's speaker and coils take Opus / s8-PCM over the same HID output plane (reports
`0x32`/`0x36`/`0x39`, see `dualsense.rs`). Every layout sounded choppy until
`device/internal/stopSniff` was called for the pad: **the TV keeps the HID link in sniff mode**, so
output reports leave in bursts at the anchor points and the pad's audio buffer starves in between —
it then replays stale buffer content, which is what "frames out of order" sounded like.
`stopSniff`/`startSniff` sit in the `public` group; call stop when the audio lane opens and start
when it closes. The coil lane alone masked this: a buzz with periodic holes still feels like a buzz.

**The two take different payloads.** `stopSniff` wants the address alone; `startSniff` wants HCI
Sniff Mode's parameters and refuses anything else with `errorCode 144`, a schema error that names
nothing. The working shape:

```json
{"address":"…","minInterval":96,"maxInterval":124,"attempt":4,"timeout":1}
```

Intervals are 0.625 ms slots and bound how long the pad waits to transmit, so they track the ~77 ms
anchor the TV's own policy used — the pad drives this app's UI between sessions, and a slower
power-saving interval would show up as input lag there. Report a refusal from the loop that CAUSED
it: replies are asynchronous and `REPLIES` is process-wide, so an undispatched one surfaces ~11 ms
into the next session and reads as its fault.

Those two are the **only** sniff-related methods in the whole `bluetooth2` API — there is no
link-policy or QoS call, so nothing persistent can be set and a re-assert is the only lever. Do not
re-assert on a timer: sniff is a link-IDLE state and a lane at 94 reports/s never lets the link
idle, so `stopSniff` rides the edge out of an idle lane (2 s floor, since the host gates audio on
silence).

**Feed the pad at its own clock, never faster.** One report per 10.667 ms, and the tick must land
on the next interval in the *future* — advancing by one interval lets a tick whose work overran
fire again at once, and each catch-up report is one the pad has no room for. Measured 110 reports/s
against the pad's 93.75 with speech breaking up audibly. Two reports per tick belong to the
pre-fill alone: keyed on ring depth they never stop, since depth after the pre-fill is clock drift,
not backlog.

## Pad audio (`0xD1`): both lanes on a Bluetooth pad

`session::pad_audio` declares `CAP_HAPTICS` (`set_pad_audio_caps` **before** the `GamepadArrival`,
and only toward a `HOST_CAP_PAD_AUDIO` host — an older host reads arrival flags as the bare pad
index) and decodes the coil lane into a rumble `Envelope` the main loop applies through the same
evdev route. **While coil frames arrive the wire rumble plane is drained but not applied**
(`pump_feedback_once`): the host forwards a title's classic rumble too, and the kernel's rumble
report also sets `HAPTICS_SELECT`, which mutes real coils. **Verified against a real libScePad
title (Spider-Man, 2026-09-03)** over a live 1440p session: coil frames with real content (envelope
peak 0.10-0.12), speaker frames alongside, and not one wire-rumble command applied for the whole
run. A game's own rumble still works when no coil frames are arriving. The chain is unity gain end
to end and `SPEAKER_VOLUME` already sits at `0x64`, the top of the range the pad honours, so
"quiet" is the title's mix, not headroom we are leaving.

⚠ `Envelope::active()` only asks whether coil frames ARRIVE, not whether they carry anything, and
it is what suppresses the wire rumble plane. A title streaming near-silence would mute a pad's
motors for its whole run and give nothing back.

The speaker lane is declared too, but **only for a Bluetooth pad** (`find_address`): the `0x36`
report over the Luna bus is its one transport, and a USB pad has no `Uniq` to find. Verified on a
G5: speech through the pad speaker is continuous, a sustained pure tone is rougher (every Opus
frame seam speech would mask).

Jail facts for the USB route: **no `/dev/bus/usb`** anywhere, so usbfs/libusb is out; **`/dev/snd`
is mounted rw** and the app's uid is in `audio`, and PulseAudio (`/var/run/pulse/native`) answers
`pactl`. Whether a wired pad's 4-channel USB-audio card appears there is untested.

## Known platform limitations (don't retry)

- **Frame rate paces the stream; the panel refresh rate cannot be set.**
  `webosbrew/SDL-webOS` exposes read-only `SDL_webOSGetRefreshRate` only; there is no set-side
  webOS API. Nothing resolves the symbol now.
- **Magic Remote Back requires `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK`** set before window creation.
  Arrives as `keycode = 2097155`. Same for Home (`..._KEYS_HOME`) and Guide (`..._KEYS_GUIDE`). The
  launcher ribbon overlay needs `SDL_WEBOS_ACCESS_POLICY_RIBBON=false` or it pops over the app.
- **Access-policy hints are latched at window creation and are all-or-nothing** — they cannot be
  scoped to the stream only.
- **A held Back arrives as the EXIT key, not a long Back — don't time the hold yourself.** webOS
  does its own long-press detection: a short tap is delivered as the Back key (keycode 2097155, no
  scancode), but *holding* Back fires webOS's EXIT gesture as a discrete
  `SDL_SCANCODE_WEBOS_EXIT = 505` press, and the held Back key never reaches the app at all. So poll
  `WEBOS_EXIT_SCANCODE` (edge-detected like the colour buttons — 505 is outside rust-sdl2's
  `Scancode` enum) and open the disconnect/quit dialog on its rising edge. A short tap stays plain:
  forwarded to the host as Esc (stream) or back-nav (menu). Needs `KEYS_EXIT` as well as
  `KEYS_BACK`, or the gesture SIGTERMs the app instead of delivering 505.
- **Gamepad disconnect shortcuts must be holds, not presses** (`runtime::input::DisconnectChord`,
  2 s). Guide, both shoulders, or Start+Back opens the in-stream disconnect dialog — and every one
  of those buttons is also forwarded as real game input, which is the whole constraint (L1+R1 is a
  common in-game binding). Chord state is tracked from transitions (SDL reports no held state
  here), **cleared when it fires or the pad unplugs** — an open dialog swallows controller events
  and an unplugged pad sends no releases, so without that the buttons stay logically down and the
  dialog reopens the moment it's dismissed.
- **A hidden window gets no pointer input.** Keep it mapped and fully transparent `RGBA(0,0,0,0)`
  each frame so the NDL plane shows through (not `.hide()`).
- **Two independent cursors** — webOS draws its own pointer, the host draws a second one over the
  network. Three levers, in the order they matter: `EVIOCGRAB` on the mouse's evdev node (starves
  the compositor of reports — the load-bearing one), `SDL_webOSCursorVisibility` →
  `wl_webos_input_manager.set_cursor_visibility`, and `show_cursor` for SDL's own cursor object.
  - **The compositor's repaint is lazy, in both directions.** `libWebOSCoreCompositor` branches on
    visibility: visible synthesizes a mouse event, invisible only *marks* the pointer and lets the
    next pointer event do the drawing. Under the grab no such event arrives, so an arrow already on
    screen survives the hide until something on an ungrabbed node flushes it; showing is equally
    stuck. `Cursor::flush` supplies the event with a warp — to screen centre while captured, to the
    pointer's own position otherwise. This is why `set_cursor_visibility` read as "does nothing" for
    four attempts; it works, and the timer-based re-assert workarounds built on that misreading are
    gone.
  - **`WEBOS_CURSOR_TIMEOUT=0`** in surface-manager's environment, so the compositor's own
    inactivity auto-hide never fires. Nothing retracts the arrow on its own.
  - **Don't guard on `is_cursor_showing()`.** `SDL_ShowCursor` reaches the Wayland backend only
    when its cached `cursor_shown` flips, so a repeat hide is a silent no-op and the query reports
    "hidden" while the TV visibly draws an arrow.
  - Motion is **not** scaled. A client-side damping factor only masked the jitter the evdev path
    below actually fixes.
- **Absolute pointer input is bounded by the panel; captured streams need relative.** webOS's
  pointer can't leave the screen, so `MouseMoveAbs` saturates at the edge. "Cursor capture"
  therefore also switches SDL to relative mode and sends `InputKind::MouseMove` deltas. webOS
  advertises no `zwp_pointer_constraints_v1`, so the SDL fork emulates relative mode by warping its
  own pointer to screen centre each motion (`wl_starfish_pointer_set_cursor_position`) — which is
  what makes the deltas unbounded. `SDL_SetRelativeMouseMode` therefore always returns 0 here.
- **A real HID mouse must bypass SDL — `/dev/input/event*` is readable from the jail.** Unlike
  hidraw, the evdev nodes are `root:compositor 0660` and the app's uid carries gid 505
  (`compositor`) — the same access the pad's `EV_FF` rumble node relies on. Motion arriving via SDL
  comes from the compositor's pointer, smoothed and resampled for a wrist-waved remote, and jitters
  in games no matter what the client does with the deltas; `platform::webos::evdev` reads the mouse
  directly instead. Constraints, each learned the hard way:
  - **Keyboards are grabbed in both cursor modes, mice only under Capture.** An ungrabbed USB
    keyboard still reaches surface-manager, which reads modifier+click as a system gesture and
    warps its pointer to screen centre — with Capture off the TV cursor and the host's then
    alternate between centre and the real mouse position on every Ctrl/Alt/Shift+click. Grabbing
    keyboards fixes it without costing the TV pointer, which desktop mode needs to aim.
  - **A grab is per node, never per event type**, so a combo keyboard+mouse node has its pointer
    forwarded too or the mouse goes dead.
  - **Keyboard nodes are `KEY_A`/`KEY_LEFTCTRL` minus a name denylist** (`LGE *`, `CHECK INPUT`, …):
    LG's virtual remotes advertise a full QWERTY keymap, and grabbing one leaves the TV unnavigable.
  - **SDL echo suppression differs by mode.** Capture on + HID mouse drops **all** SDL pointer
    events; Capture off drops only motion, and only within the keyboard's recency window, since its
    clicks are the real ones.
  - **webOS 23+ types every pad press as a remote key too** (arrows, OK, Back), and a Wayland key
    names no device, so SDL cannot tell the echo from the Magic Remote. Measured on a G5 (10.3.1):
    the pad's presses appear only on its own node, the remote's only on `LGE M-RCU - Builtin [0]`
    (Back = `KEY_PREVIOUS`), so the stream reads that node ungrabbed and admits a key only when the
    remote pressed it within 250 ms (`RemoteGate`). Until such a node is opened every key passes.
    Re-arming SDL's `cloudgame_active` on focus changes did nothing in four A/B runs.
  - **SDL relative mode must be off** — the fork warps its pointer per motion event, a thousand
    pointless compositor round-trips a second.
  - **Device filter is `EV_REL` with `REL_X`/`REL_Y` and *not* an absolute pointer** — test `ABS_X`/
    `ABS_Y` specifically, since real mice advertise stray absolute axes (a Logitech receiver reports
    `ABS_VOLUME` for its media keys).
  - **`EVIOCGRAB` is scoped to `HidInput::set_active`, not held for the reader's life.** The kernel
    releases the grab the moment our fd closes (including on panic), so a wedged reader costs "no
    HID input", never a TV-wide dead mouse. The same flag gates whether the reader calls its `sink`,
    so "grabbed" and "forwarded to the host" can't drift apart.
  - **Hot-plug rescans are gated on `/dev/input`'s mtime.** Opening a node costs ~40 ms on this TV
    and ~20 nodes are empty (`ENXIO`), so an unconditional rescan stalls the reader for most of a
    second.
- **Colour buttons: Green/Yellow/Blue need raw scancode polling, Red does not.**
  `SDL_SCANCODE_WEBOS_{RED..BLUE}=486..489` exist in the fork, but only 487..489 ever appear in the
  keyboard-state array. Red instead arrives like Back does: a plain `KeyDown` carrying **keycode
  2097169** and `scancode: None`. So poll the other three and match Red as a keycode. There is no
  `ACCESS_POLICY_KEYS_*` hint for colour keys.
- **The keyboard Win key is Home-class**: it is gated with the remote's Home under `KEYS_HOME`.
  Capture both, and relaunch the launcher via luna on the remote-Home keycode so Win reaches the
  host.
- **Thread priority boosting was built and removed — don't re-add it.** Renicing hot threads needs
  `CAP_SYS_NICE` or a large enough `RLIMIT_NICE`, and a Dev-Mode SAM jail grants neither (raising
  the soft limit to the hard limit needs no privilege, but the hard limit is already too low). The
  poller that hunted NDL's vendor GStreamer pad threads also cost a 100 ms-interval thread for up
  to 5 s during connect — the busiest window on a 3-core SoC. All threads run at the same priority.
- **Don't toggle window show/hide while NDL composites.** It silently kills the process
  (uncatchable Wayland crash). Test visibility changes in isolation.

## Runtime gotchas (LG CX/G5)

- Apps install to `/media/developer/apps/usr/palm/applications/<appid>/` = `$HOME` (writable dir
  for logs, `settings.json`, the art cache and the client identity PEMs).
- `luna-send` over raw ssh **needs `ssh -tt`** (a real PTY) or output is silently swallowed — the
  task targets go through `ares-install`/`ares-launch`, which don't have this problem.
- **Black screen despite decode**: launch through the real app lifecycle
  (`luna-send .../launch`, SAM jailed uid). NDL punch-through only composites for a SAM-managed
  foreground app.
- No env vars in a SAM launch, but `params` in `applicationManager/launch` reaches a native app as
  argv[1] JSON (parsed by `logger::launch`).
- SDL2/Wayland may report `refresh_rate=0` — clamp to a sensible default.
- **Game mode / ALLM is rooted-only**: the public bus denies `settingsservice`, so the picture and
  sound mode change routes through hbchannel root exec and the Settings row is shown only on a
  rooted TV.

## ChaCha20 over AES-GCM

CX/G5 are 32-bit userland on ARMv8-A. RustCrypto's `aes` crate has ARMv8 intrinsics for `aarch64`
only; 32-bit ARM falls back to software regardless. ChaCha20 (add/rotate/xor, no crypto
instructions) stays fast. Advertise `VIDEO_CAP_CHACHA20` unconditionally in `session::connect` —
it's the only cipher this client speaks.

## Large library handling

- **The cover window is O(visible)**: `app::render::prepare_grid` requests art only for rows within
  `CARD_PREFETCH_ROWS` of the viewport and evicts outside a deliberately larger `CARD_KEEP_ROWS`
  (hysteresis stops oscillation), and only when that window moves.
- **Cover art**: `ArtLoader` request/response (the UI asks for visible covers, forgets scrolled
  ones). Cached on disk as *encoded* bytes (`$HOME/art-cache/`, write-then-rename). Failed decodes
  are deleted.
- Effect at 365 titles: decoded covers drop from 365 to the viewport window (~5 columns).

## Audio: two routes, one pipeline (SDL is the default)

`Settings` → **Audio** → **Audio processing** picks the route (`core::model::AudioRoutePref`), and
both are built on the same pipeline: `session::audio::AudioStage` decodes (or forwards) into
whatever `core::media::AudioSink` the route selected, and one pump drives it. Adding a third route
is one `AudioSink` impl.

| Route | Label | Path | Layouts |
| --- | --- | --- | --- |
| `Software` (default) | Software (SDL) | libopus here → SDL device, NDL's clock plane on its metronome | up to 7.1 |
| `NdlOpus` | Offload (NDL) | Opus decoded by the TV; 5.1 re-encoded into NDL's layout first | 2, 5.1 |

**Why software is the default.** NDL paces the picture against a *fed* audio plane, so a plane fed
from the network inherits the stream's arrival jitter — which is the stutter the silent clock plane
was introduced to cure. The offload route is shorter and stays selectable for exactly that
comparison; the overlay names which one ran (`Opus SW` / `Opus HW`).

**Offload is also the surround route.** NDL decodes stereo Opus and exactly one 5.1 layout: the
standard coupling, `(FL,FR)+(RL,RR)` with FC and LFE mono (`AudioLayout::Standard`,
`ndl::OPUS_51_LAYOUT`). Every session asks the host for it (`Hello::audio_layout`); a host that
knows it encodes it, and the plane is fed the wire untouched. An older host answers legacy —
`(FC,LFE)` coupled — and `session::audio` decodes and re-encodes into NDL's layout, one 5 ms packet
per frame. Some sets accept the load and then play nothing, which no runtime probe detects, so it
stays a choice rather than the default.

**Surround reaches a receiver only through the Opus plane.** Measured on a G5 into a Denon
AVC-X3800H over HDMI, with NDL reporting multi-channel PCM `Supported`: the SDL route plays into
PulseAudio's one hardware sink, `pcm_output`, which is s16le 2ch; and a plane loaded as 6-channel
PCM confirms and plays, but the AVR reports PCM 2.0. aurora-tv dropped its NDL PCM 5.1 path too.
Read the AVR, not the ears: a Denon answers `OPINFINS ?` on telnet port 23 with one digit per input
channel (`2` = present).

**The offload route exists only under NDL v2.** v1 (webOS 4 and below) has no audio type at all, so
`caps::VideoCaps::audio_plane` is false there and `AudioRoutePref::available` collapses to
`Software` — the row locks, and `Settings::clamp_to_caps` rewrites a document carried over from a v2
set. The Audio row's layouts follow the *selected* route, so picking Offload offers stereo and 5.1,
not 7.1.

**Nothing is ever mixed down, and the layout row is a preference.** `Settings::audio_channels` says
"5.1 where it can play"; `Negotiated::clamp` is the one place it becomes a width on the wire,
narrowed by what the selected route carries (`AudioRoutePref::max_channels`). So a layout the route
can't carry is never encoded, sent or decoded, this client never folds, and the preference survives
a route change instead of being rewritten out of the document (`menu::audio_row_channels` shows the
preference held down to the route). A width mismatch at `AudioStage::new` is an error, not a
downmix.

- **Sound Out narrows nothing.** `NDL_DirectAudioSupportMultiChannel` says whether multi-channel PCM
  leaves the set *right now*. It reads "will play" only with Sound Out on Pass Through, and it
  describes NDL's own PCM path, not the SDL device the software route plays through — so gating the
  handshake on it turns a 5.1 pick on a default-configured TV into a stereo session. The client asks
  for the chosen layout and lets webOS fold. `ndl::log_audio_output` logs the answer at connect for
  sessions wider than stereo: the first line to read when 5.1 sounds like stereo.
- ⚠ **`NDL_DirectAudioSupportMultiChannel` has an out-parameter**:
  `int NDL_DirectAudioSupportMultiChannel(int *isSupported)`, returning 0/-1, with the code written
  through the pointer — `0` unsupported, `1` no device, `2` device but not passthrough, `3` will
  play. Reading the *return* as the code (as this client did) is both wrong and UB on ARM EABI: the
  callee writes through whatever `r0` held. The `NDLMultiChannelPCMCallback` codes documented beside
  it are the same ladder shifted down by one; they are not interchangeable.
- ⚠ **Optional NDL symbols must be probed after the library is open.** `RTLD_DEFAULT` finds nothing
  until something has `dlopen`'d `libNDL_directmedia` — and the capability probes run at startup,
  before any decode session. `ffi::optional_sym` forces `ffi::common()` first; without it every
  optional symbol reads as absent and the TV silently loses 5.1.
- ⚠ **NDL's reported `maxBitrate` is unreliable** — it is an SoC tier, not a link limit. Don't cap
  the stream with it; aurora-tv ignores it on webOS too.
- **Samples are converted only where NDL needs its own layout.** libopus decodes straight into f32,
  which is exactly what the SDL device takes; offload forwards stereo Opus untouched and re-encodes
  only 5.1. There is no second buffer on the SDL route.
- **The software route's latency is buffering, not decode.** Software Opus is 5% of a core, so the
  only client-side terms are the ring depth and the device quantum.
  - **The prime overshoots, and the shed is what takes it back.** The ring is inspected once per
    callback, so it crosses the target somewhere inside a 10.67 ms period in 5 ms steps — first
    serve is target+0..15 ms. `JitterPolicy`'s drift shed handles that and every later source of the
    same drift (host capture clock vs. this DAC) by walking the depth back to target one crossfaded
    5 ms frame at a time. Letting the policy own priming is cheaper than second-guessing its state
    machine for one transient.
  - **The device quantum is logged at open** (`SDL audio device:`). SDL may negotiate something
    other than the requested 512 frames, and a larger one silently raises the policy's effective
    target, which is floored at `one callback + 5 ms`. Read that line before concluding anything
    about this route's latency.
- **The SDL ring runs `punktfunk_core::audio::JitterPolicy`** (`JitterTuning::AAUDIO`, unmodified),
  the same de-jitter state machine the Linux, Windows, Android and Apple rings use. Prime to an
  adaptive target, grow it only on a set that actually underruns, and walk drift back down one
  crossfaded 5 ms frame at a time. `crossfade_drop` fades BOTH corrections — the smooth shed and the
  hard-cap trim.
  - ⚠ **This was removed once and restored deliberately.** The removal was credited with "~35 ms of
    floor", and that was wrong: the old local preset was 25/90 ms and the fixed prime that replaced
    it was also 25/90, so no floor was ever saved. What was lost was jitter resilience — the
    adaptive floor and the crossfaded shed (replaced by an uncrossfaded drop of up to 65 ms, an
    audible click) — on the fleet's worst link. Do not delete it again without numbers from
    `audio playback (SDL device)`.
  - **The preset is `AAUDIO` rather than a local copy.** Field for field it already *is* what the
    local tuning was, with the old `deprime_after: 5` **callbacks** now expressed as
    `deprime_ms: 60` (a callback count means a different span on every device). AAudio's rationale
    (raw callback, client owns the buffer, Wi-Fi power-save bunching arrives as underruns) is this
    TV's situation exactly.
  - **The A/V sync loop is not wired.** `set_sync_target` is never called, which core documents as
    reproducing unsynchronised behaviour exactly. It never steered here anyway, and the video
    reference this platform can build is biased low by NDL's unobservable decode+panel term.
  - **Read `target_ms` in the debug line.** It is the adaptive floor's current answer, and the one
    figure that says whether this set needed more than the 25 ms base. `sheds` vs `trims` separates
    "drift corrected inaudibly" from "the link outran the headroom".

**Blind alleys, so they aren't re-tried:**
- ⚠ **The NDL PCM plane was built, measured and removed.** A third route decoded Opus here and fed
  NDL's `NDL_AUDIO_TYPE_PCM` plane. Fed on arrival it made the plane's depth — the thing NDL paces
  the PICTURE on — a function of network jitter, and the field report was intermittent lag. A paced
  ring in front of it fixed that, and what was left was a **small** latency win for a route that
  could never carry 7.1, whose `"6-channel"` interleave order was inferred and never verified on a
  set. Not worth a third hardware path: for stereo the offload route is shorter still, and for
  anything wider software is the only route that plays it.
- **`sdl2::audio::AudioQueue` cannot carry a de-jitter policy** — `queue_audio`/`size`/`clear` and
  nothing else, no partial drop. That is why the pull callback stayed.
- **Do not put the audio drain back on the main loop.** It was there because `AudioQueue` is
  `!Send`, which put the audio cadence behind the UI's software rasterizer.
- **Do not shrink `DEVICE_BUFFER_FRAMES` below 512** to chase latency: a smaller quantum on this SoC
  buys more wakeups and more missed callbacks.
- **Do not split `lock_ffi` per plane** without device evidence. No NDL entry point is documented as
  thread-safe; the contention between the video feed and the audio bursts is real but a second guard
  is a guess about vendor internals.
- **Do not fold the clock plane's keep-alive into the audio pump.** Its cadence is 20 ms and the
  pump parks up to 100 ms on an empty transport; one thread would mean a starved plane, i.e. the
  stutter the plane exists to prevent.

## A/V sync

The host stamps `pts_ns` on every audio datagram, and with audio on NDL's plane **NDL does the
synchronisation**: both planes are stamped in one timeline. The client-side `AvSync` estimator went
with the jitter ring it existed to steer — there is no ring depth left to move.

What still matters:

- NDL is submit-only (`NDL_DirectVideoPlay` reports nothing about presentation), so glass time can
  only be estimated and the decode+panel constant after the render queue drains is not observable
  from the app at all.
- ⚠ **Use `frame.pts_ns`, never the paced value**, wherever a host-clock comparison is made. Both
  are in scope at the submit site with near-identical names; the paced one has been mapped into
  NDL's player clock by `session::timeline::Pacing`.
- ⚠ **NDL can fail the whole load asynchronously, and then never recovers.** Seen on a CX: load
  state `0x12` with `errorCode 600`, after which every `NDL_DirectVideoPlay` returns -1, the clock
  plane's `NDL_DirectAudioPlay` fails too (so the thread that paces the picture exits for good) and
  NDL reports `UNLOADCOMPLETED` on its own. There is no in-session reload path, and a re-anchor does
  nothing for a lost pipeline, so the client spun on failed feeds with a frozen picture while the
  QUIC session stayed perfectly healthy. `ndl::fatal()` latches that state, `VideoSink::is_dead`
  carries it up backend-blind, and the stream loop ends the session. What PROVOKES the 600 is still
  unknown.

## NDL's audio plane: why every load has one

⚠ **NDL only paces the picture when the load HAS an audio plane and that plane is PRIMED.** On a
video-only load it ignores presentation timestamps entirely and presents at feed cadence, which
beats against a 120 Hz panel — the long-standing "smooth at 1080p, randomly smooth above it"
stutter. Measured on a CX: frames stamped ~60 ms ahead of the player clock still left
`render_buffer_length` at 0-1 and still stuttered, and the same session with an audio plane was
smooth.

**It does NOT need that plane to keep being fed** (CX, 2026-09-16). With the plane left 136 seconds
stale — across an idle stretch where the host sent no frames either — the picture paced normally
and the deadline margins were slightly better than with a metronome running. This is why the
silent metronome is gone: the load prime is what matters, which is also aurora's structure (one
empty Opus frame at `LoadMedia`, `ndl_player.c:225-227`, and nothing after). Evidence is one set;
`ndl_plane_feed=continuous` restores the metronome without a rebuild.

So **every accepted V2 load asks for a stereo audio plane**, and what rides it is a separate
question:

- **Software decode** (the default) — `platform::webos::audio` decodes the real audio to SDL and
  the NDL plane carries only its load prime. `NdlVideo::run_clock_plane` still runs, but it now
  only watches (the unconfirmed-plane check) unless a launch param restores the metronome.
- **Hardware Opus decode** (Audio processing → Offload, opt-in) — the audio pump feeds the real
  stream, stamped on the video timeline; no SDL device is opened.

`run_clock_plane` runs on **both** routes (`session::pipeline::spawn_plane_threads`), and with the
metronome retired its remaining job is the unconfirmed-plane check, kept off the feed path. The
dead-capture filler went with the metronome: a starved plane turned out not to freeze the picture.

A set that refuses the audio plane outright ends up video-only at the load and gives up pacing with
it; the session log names which route it took. **NDL v1 has no Opus audio type at all**, so webOS 4
has no pacing reference.

Blind alleys, measured and ruled out: NDL's standing cushion depth (`render_buffer_length` is 0-1 in
smooth AND stuttery sessions), feed lateness (45-60 ms of slack either way), software Opus decode
cost (5% of a core, `dropped=0`), and HDR mode re-entry (a real bug, fixed in *Video decode* above,
but not this one).

### Wiring the plane

Byte-exact with `mariotaku/ss4s` `ndl/webos5`: `NdlAudioConfig.sample_rate` in **kHz** (`48.0`, not
`48000.0`), the stereo `opus_empty_frame_211 = {0xec,0xff,0xfe}` decoder prime, and combined
audio+video in one load. Struct layouts (`NDL_DIRECTMEDIA_AUDIO_OPUS_INFO_T`, `..._DATA_INFO_T`)
match `webosbrew/webos-userland` field-for-field, including the explicit trailing `_padding` — the
whole struct is memcpy'd into a fixed-size union arm, so **any implicit padding in a `repr(C)`
struct handed to NDL is uninitialized stack on the wire**.

⚠ **Keep NDL init lazy inside `load()`.** An ss4s-style warm init at startup produces "player is not
loaded" on the first load.

⚠ **`LOADCOMPLETED` is not reported against the same thing on every set, so it cannot be the test
for the audio plane before a frame is fed** (measured, issue #188). A 2025 QNED (webOS 10, `k24n`)
reports it **26 ms after the first access unit reaches the decoder, and never before**: its
video-only load misses the 2 s `LOAD_COMPLETE_TIMEOUT`, then confirms the instant `ensure_loaded`
gives up and feeds. A CX reports it ~40 ms after the load with no frame at all. The old code judged
the plane inside the load wait, where nothing can feed a frame — the pumps do not spawn until
`session::connect` returns — so on the QNED every session read a healthy plane as refused, fell back
to video-only, and ran unpaced. That is #188's delay; longer waits don't help.

So the plane is **asked for and then kept, confirmed or not**. `AUDIO_PRIME_BUDGET` (500 ms) buys
only the fast confirmation a CX gives; a load that does not answer inside it is taken anyway and the
metronome carries on feeding the plane, which on an ingest-gated set is what eventually produces the
callback. `run_clock_plane` is therefore **deliberately not gated on `LOADCOMPLETED`** — it is the
prime's continuation, and gating it would leave the plane unfed from the end of the prime until the
first video frame. On the QNED that is seconds, and it covers the ~100 ms of ingest that sets NDL's
standing present cushion.

⚠ **There is no mid-session verdict and no in-session re-load.** One was written for #188 and
reverted: it could not be right, because the plane question is resolution-independent (NDL's
`VideoInfo` carries no framerate at all) while #188 is 4K120-HDR-only on a set where 4K60 and
1440p120 are fine. It also had to re-apply HDR metadata to the new pipeline, which is the mode-drop
documented under *Video decode* — the fallback could cause what it was meant to fix. A load whose
audio config the pipeline rejects outright still fails at the load and falls back there, which is
the only fallback that exists.

What survives of the verdict is the **log line**: `v2::PLANE_CONFIRM_GRACE` (750 ms past the first
accepted frame) is where an unconfirmed plane gets named. That window is the one place the two
readings separate — before a frame, "no callback" is both a healthy ingest-gated set *and* a
pipeline that rejected the Opus config asynchronously and accepts every frame into a decoder that
never runs. Nothing is recovered, deliberately: a set that does this has not been measured, and
acting on the guess is what #188 already cost. If that WARN shows up in a report, THAT is the set to
build a fallback for.

The route is picked from the PROVEN plane (`AudioPlane::accepts_stream`), not from one that was
merely asked for: an unconfirmed plane paces the picture perfectly well, but the session's only
audio must not ride one that may never work, and the route cannot be re-picked once the stream is
running. Only a session that means to put REAL audio there pays for the answer: `plane_budget`
charges the offload route `AUDIO_PROVE_BUDGET` (2 s) and every other session the short one, since on
an ingest-gated set the extra wait is pure black screen. A downgrade off offload is worse than either
route on its own — `max_channels` has already clamped the handshake to stereo on the strength of the
request — so the `audio path:` line names it explicitly. Both load waits bail early on
`ndl::fatal()`. Every path that loses the plane warns that the picture will not be paced; grep the
log for that line before theorising about any later delay.

The REAL audio feed gates on the `LOADCOMPLETED` latch itself, never on `feed_unblocked`: that flag
is the VIDEO feed's gate and latches optimistically once frames have to flow regardless. Real audio
on a plane NDL has not confirmed costs the session its sound outright — whereas silence on one is
free, which is why the two feeds gate differently.

The load blocks `session::connect` between the handshake and the first `next_frame`, so anything
timing a launch has to cover it. `app::hero` is fine (its `FIRST_FRAME_WAIT` only starts once
connect returns, under a 30 s `HERO_LOADING_MAX` backstop), but `hdr_pattern`'s `PRESENT_DEADLINE`
runs from `Playback::start` and must stay above the whole sequence.

⚠ **The prime's stamps and the player clock share one origin — `load_instant` is the load CALL**,
where NDL's PTS domain starts. It used to be stamped *after* the load wait, so the two domains
differed by the load's duration D and every consumer carried a correction for it — worst, the
offload route's real lead became `PLANE_LEAD_MS − D`, i.e. ≈ 0 on a CX. One origin removes them
all. `last_real_feed_ms` is seeded with the clock at construction, not 0, since it already reads D.

⚠ **The silent metronome's cushion is `METRONOME_LEAD_MS` (80 ms), not `PLANE_LEAD_MS` (40 ms), and
the arithmetic must be done in one domain.** 80 ms is the depth 4K120 5.1 smoothness was actually
confirmed on (`plane_lead` read 120 on a CX only because it was taken against a lagging clock), so
the constant is pinned there rather than falling out of how long a TV took to load. Under 80 is the
known stutter risk; over it is cheap — a silent plane costs no lip sync — so it IS the knob for a
set still stuttering at high refresh: walk it UP against `plane_lead`. The offload route's *fill*
still targets `PLANE_LEAD_MS` plus whatever extra lead the Smoothness buffer asked for
(`set_plane_extra_lead_ms`), which must match what `play_audio` targets or resuming real packets
floor onto the fill's ceiling.

⚠ **The prime is what completes the load.** An audio-enabled load does not report `LOADCOMPLETED`
until its audio plane has received a packet — but the pumps that would send one don't spawn until
`session::connect` returns. That deadlock read as a whole session of black picture with working
sound, no error anywhere. `NdlVideo::prime_audio` feeds bursts of empty frames through the load
window itself; on a CX that turns "never" into `LOADCOMPLETED` in ~40 ms. The prime's highest stamp
seeds `last_audio_pts_ms`, so the first real packets are floored rather than read as a rewind.

⚠ **Never flush a pipeline that has not finished loading — it kills audio for the session.**
`NDL_DirectVideoFlushRenderBuffer` before `LOADCOMPLETED` takes the audio plane out permanently;
video recovers and gives no sign. `ensure_loaded` returns a typed not-loaded error and the sink
holds + requests a keyframe **without** flushing. Nothing is queued at that point anyway.
`NDL_DirectAudioPlay` returns 0 either way — there is no error to find on the audio side.

⚠ **A hold must never be the response to `NotLoadedYet` alone.** Making it trigger
freeze-until-reanchor skipped the `play()` call that holds the feed-anyway escape, and the session
deadlocked into a black first frame — worst on a static desktop, where no new IDR arrives on its own.

⚠ **Never drain NDL's queue with a feed hold.** An emptied present cushion breaks pacing for the
rest of the session; trim the stamps instead (#188).

⚠ **Audio stamps must never go backwards — NDL reads a rewind as a mute for the rest of the
session**, and does not resync. `NdlVideo::play_audio` and `NdlVideo::burst_silence` are the only
feed points, both serialised under `lock_ffi` and both flooring at `last_audio_pts_ms` — the floor
must be read under that guard, or a packet measured against an older ceiling blocks on the lock and
then hands NDL the stale stamp.

⚠ **The audio plane stamps off the PLAYER clock, not the host's.** It used to map the host capture
PTS through a shared session clock, with a per-latch skew lifting each resumed run above the ceiling.
That **ratchets**: a freeze-until-reanchor stalls the mapped timeline while packets keep arriving,
the resumed run lands below the ceiling it already reached, and the only monotonic repair is to add
lead — which nothing in the session can ever pay back. Field case (CX, offload on): five re-anchors
inside four seconds walked the plane from 78 ms to 124 ms of lead. Audio is now stamped
`player_clock + PLANE_LEAD_MS` and the host PTS is ignored (`AudioSink::feed` takes it and drops
it): a wall clock advances at the same rate whatever the host PTS does across a freeze, so
`last_audio_pts_ms` is left with nothing to do but absorb reordering. The clock plane targets the
same figure, so the two feeders share the ceiling without either driving it.

⚠ **The ratchet was real and was NOT the mute.** Measured after the change, under a deliberately
saturated airlink (276 Mb/s of competing download against a 188 Mb/s stream): 32 re-anchors,
`plane_lead` pinned at 37-40 ms, stamps provably monotonic and evenly spaced — and the audio still
died permanently. Do not spend another round on stamp arithmetic.

⚠ **The loss hold no longer flushes, and THIS is what was muting the plane** (confirmed on device).
It was the last structural difference from `ss4s`, which never flushes mid-stream — its only
recovery is unload+load, and it does not lose its Opus plane. Every flush stops the pipeline: each
one used to be followed by `NDL load state: PLAYING (0x1a)`, a transition NDL only makes from
not-playing. Confirmation (CX, same storm as above): 16 re-anchors, holds up to 2 s, **not one
`PLAYING` transition in the whole log**, `plane_lead` 38-40 ms flat, and audio intact — where the
identical storm against the flushing build killed it permanently.
`NDL_DirectVideoFlushRenderBuffer` is safe to call and reports success; what it costs you is the
audio plane, silently, for the rest of the session. The decode-error path still flushes, where the
pipeline has actually errored; loss is a network event and NDL's queue holds good frames the hold is
about to present anyway. `last_base_ns` survives the pacing reset accordingly: without a flush the
pipeline still holds everything fed before it, and a run restarting from 0 would walk the video
stamp backwards.

A loss hold is also **lifted by a reanchor alone** — waiting for anything more left holds open past
their cause.

This is where `mariotaku/ss4s` ended up too, from the other direction: `734e643` added a thread
feeding empty Opus frames through gaps, then `ef0c0ae` deleted the whole mechanism and moved both
planes onto `CLOCK_MONOTONIC - mediaLoadedTime`. moonlight-tv#493 ("Stream loses audio after network
hiccup") is the unfixed version of this failure — same symptom, Opus route only, PCM never
reproduces it, only a full restart recovers.

⚠ The audio-enabled load returns success even on a TV that then plays nothing, so **no runtime probe
can distinguish the two**. If a model regresses, the `NDL load state:` log says whether the pipeline
ever started, and turning Audio processing back to Software is the way out of the hardware-decode
half.

## Cadence pacing and the present cushion

The video feed is copy-free — core reassembles one contiguous `Vec` (which `NDL_DirectVideoPlay`
requires) and the sink passes that pointer straight through, no Annex-B rewrite, no client-side
queue. Pacing is therefore only about *when* bytes are released.

`session::timeline::Pacing` wraps **`punktfunk_core::phase::CadenceClock`** (the same loop the
desktop/Android/Apple presenters pace on, so every client computes the same statistic): a type-2
loop over `ready − pts` whose cushion is `2 × measured MAD`, floored at 0.5 ms and **capped at one
frame interval** — that ceiling is core's invariant, not a knob. `snapping()` tuning, whose
rationale ASSUMES the sink latches to the panel's grid so the snap-up already carries ~half a
refresh of slack. **That assumption is unverified on NDL** — no claim of it exists anywhere in
upstream ss4s, aurora's software grid observes no display phase, and NDL's own scheduling is not
documented. Until it is measured, read the tuning
as a design assumption rather than firmware behaviour.

The mapping it replaced (a fixed anchor `base = player0 + (host_pts - host0)` plus a one-off lead
trim) is gone. It carried no rate term, so two free-running crystals walked the session's real lead
away over minutes with nothing to pull it back, and its whole jitter margin was 4 ms — below the
arrival spread of an ordinary link, so the latest-arriving frames of every window were stamped in
the past. On a CX at 1440p120 it stamped ~17% of frames late against the loop's ~7%. That
combination is the "stutters here, looks fine on the host's own monitor" report.

- ⚠ **It smooths the offset, never the timestamps.** Core tests that
  (`preserves_source_cadence`): a game genuinely rendering at 45 fps still looks exactly as
  irregular as it is. Only the transport's contribution is removed.
- What no client-side work can fix: a stream rate that is not the panel rate or an exact divisor of
  it. 60 on 120 is fine; 50 on 60 is arithmetic.
- **Optional extra headroom is the Smoothness preference** (`PresentPriority`): it **adds** 1-3
  source frame periods **on top of** the adaptive cushion, and needs a timestamp clock plus an
  accepted audio plane (NDL v2) or it falls back to lowest latency with a warning.
  ⚠ It used to SUBSTITUTE the fixed budget for the adaptive one, which made the first step a no-op
  wherever the adaptive figure was already at its ceiling — at 120 Hz that is any link with more
  than ~4 ms of MAD, i.e. most of them, and it is why the setting read as doing nothing. Additive,
  every step is worth a whole period. The knob is the only way past core's one-period cap.
- ⚠ **The cushion's ceiling is the STREAM mode's interval, never the panel's.** The two agree on
  most panels but are different quantities: the cushion bounds how long a frame may be HELD, so it
  must follow the cadence the host produces (core's own test says so). A 120 fps stream on a 60 Hz
  panel would otherwise license twice the hold the source can justify.
- **One picture folds ONCE.** Slice-progressive delivery repeats an AU's host PTS across its pieces
  at increasing arrival times, so mapping per piece teaches the loop the AU's *tail* arrival and
  inflates measured jitter by the AU's own transmission time. `VideoStage::au_base_ns` holds the
  stamp while the AU is open — which is also what makes every piece of one AU carry the same
  timestamp, as NDL (start-code boundaries, no AU flag) needs.
- **Folded at arrival**, which is where core wants it, so the estimate sees the arrival process the
  transport actually produced. `snapping()` permanently: re-tuning to `free_running()` needs VRR
  measured live off on-glass stamps this platform does not have. `note_off_cadence` IS wired here, though
  nothing on the wire marks a frame off-cadence: this client infers it from two consecutive AUs
  carrying the same host PTS (a compositor header stamp, a driver burst), because folding a
  zero-interval sample would teach the loop an arrival gap the source never had. It returns
  `ready + cushion`, which is also the only answer that advances — the anchored stamp would repeat
  the previous picture's, and NDL truncates both to one millisecond.
- **Re-anchor triggers**: the freeze-until-reanchor hold, via `reset_timeline`. The source interval
  is snapshotted at pipeline build, so if mid-session mode changes ever become a real path here,
  that snapshot is the thing to fix.
- **The stamp sequence is clamped monotonic per run** (`last_base_ns`), because the cushion can
  shrink between frames and NDL reads a rewind as a permanent session mute. It is the one invariant
  here whose violation costs a session its audio outright.
- `late_stamps` — frames whose actual stamp was already behind the player clock, i.e. the judder,
  counted — is reported as `pacing:` on the video heartbeat and `Pace` on the overlay.
- **A/V offset is `plane_lead − cushion`** (`av=` on the heartbeat, `av ±N ms` on the overlay),
  differenced in the video pump, which already holds both halves. Both planes stamp on NDL's one
  `elapsed_ns` clock, so the subtraction is legal: audio sits a fixed `PLANE_LEAD_MS` ahead of it,
  the picture its mapped cushion ahead. Positive is sound behind picture. ⚠ Offered ONLY where real
  audio rides the plane — on the software route the plane carries the silent metronome, which costs
  no lip sync, and the figure would be a fiction. ⚠ Stamp domain: NDL's decode and panel transit are
  not observable from the app and bias the picture later, so the true offset is smaller than this
  reads. Trend and sign, never calibration. ⚠ The Smoothness buffer does NOT move it: the same budget
  is handed to the plane (`NdlVideo::set_plane_extra_lead_ms`), so both halves shift together and
  the figure stays at `PLANE_LEAD_MS − adaptive cushion`. Left unmatched it would have walked
  straight through zero — Smooth 2 at 60 Hz is already 33 ms against a 40 ms plane lead — and sound
  ahead of the picture is the more audible direction.
- ⚠ **No diagnostic may take NDL's FFI lock.** `render_buffer_length` sits behind the same guard as
  the picture's own `video_play`, so a figure queried for the overlay would stall the next feed on
  the video thread. The backpressure control path already samples the depth every `BACKLOG_SAMPLE`
  (500 ms); `backlog=` on the heartbeat and the overlay read **that** sample, never a query of their
  own. The figure is therefore up to one interval old, and goes stale for the length of a hold
  (sampling is suspended while holding) — `holding` is published beside it and says so. Every other
  per-frame figure (`feed_us`, `late_submit`, `min_slack`) is gated on `timed`; the only ungated
  clock read on the feed path is the pacing input itself.
- ⚠ **Live counters belong on the stats overlay, not in the log.** A periodic dump of them buries
  the events worth reading (holds, plane refusals, slow feeds), so the `pacing:`/`video:` lines sit
  at TRACE — one step below the `TELEMETRY_LEVEL=debug` a deploy usually runs at. Everything they
  carry is on the overlay live: `pace cushion · jitter · late · slack`, `av stamp`, `plane_lead`,
  `backlog`. Raise to TRACE only when the screen is not in front of you.
- **`cushion` is the only overlay figure that moves with the presentation setting.** `jitter` is the
  measured residual and is independent of the cushion by construction; `late` is cumulative from
  session start. Watching either to see whether Smoothness is doing anything reports nothing — that
  is what made the setting look inert.
- **`min_slack` is the complete-AU deadline margin** (`Pacing::note_submitted`,
  a minimum rather than a mean, so one bad frame stays visible, taken and re-armed on the line that
  prints it — the take IS the re-arm, so reading it anywhere else shortens the window). The loop folds an AU's FIRST
  piece only — deliberately, since re-mapping per piece teaches it the tail arrival — so it never
  sees when a slice-progressive picture COMPLETED. Persistently negative `min_slack` while `jitter`
  reads healthy is the signature of a large AU finishing against a deadline its first piece set.
  Slice-progressive fires above ~25 Mb/s (core emits an early part only past a completed FEC block,
  ≈22 KB), and keyframes always split, so this is reachable at ordinary settings. Read it against
  `parts=` on the video line: `parts=0` means the whole lever is inert on that mode. ⚠ Only ever
  populated while the feed is timed (`report_decode_latency || diagnostics`), so it is a diagnostic,
  never a steering signal — a control loop built on it would silently run open-loop.
- ⚠ **The stamp is ceiled to whole ms in BOTH intents**, where the rounding used to live inside the
  Smoothness branch. NDL truncates either way, so rounding down spent cushion; generalizing it is a
  (sub-millisecond) behaviour change to Lowest latency that rode in on a Smoothness fix.

**Slice-progressive feed (on, every NDL v2 session).** Without it the decoder sees byte 0 of a frame
only once that frame's LAST datagram lands; at 200 Mbps a keyframe is many datagrams and the tail of
that reassembly wait is pure latency. `session::stage::parts` implements core's contract — parts in
order, an `offset` mismatch or a new `first` over an open AU means that AU died — and reports the
break as loss, which puts the sink into freeze-until-reanchor and asks for a keyframe. Per-frame
reference points (the decode report, the audio latch) are skipped on a piece that is not the AU's
last, since a piece is not a presentable frame.

⚠ **NDL has no `PARTIAL_FRAME` flag and no AU-boundary flag at all** — it takes raw Annex-B and must
be finding boundaries by start code, which is the whole reason to expect a fragmented feed to work,
and the whole reason it might not. Clamped to NDL v2 (v1's feed carries no timestamp to repeat
across pieces). Failure mode is visible corruption plus `frame parts:` warnings; there is no toggle,
so a regression means reverting `Negotiated::clamp`'s `frame_parts`.

⚠ **Real audio on the plane must carry a lead, or the PICTURE stutters.** The plane's queue depth is
what NDL's audio renderer paces the video plane against. The offload route makes real packets the
only feed, and fed straight off the wire they stamp at ≈ the player clock: depth ≈ 0, renderer at
the edge of underrun, picture stutters on network jitter. Fixed by `PLANE_LEAD_MS` (40 ms), added to
every real stamp in `play_audio`; the clock plane's fill targets the same figure so neither pushes
the other's ceiling. NDL takes no depth argument, so a stamp in the future is the only way to ask
for one. Cost is lip sync, `PLANE_LEAD_MS` behind the picture — walk it down on device against
`lead` on the overlay's audio line and `plane_lead=` on the video heartbeat, the only places the
depth is observable.

- Unknowns, in order: the depth NDL holds on that plane (it is not `render_buffer_length` and there
  is no query), and whether offload beats software on a set where offload works at all. Both routes
  are named on the overlay (`Opus SW` / `Opus HW`) and in the `audio path:` log line precisely so a
  report says which one produced the numbers.
- Not tried yet: **phase-locked capture** (core has the protocol — `report_phase` +
  `CLIENT_CAP_PHASE_LOCK` — and the host aligns its capture tick to the client's panel grid, which
  *reduces* latency instead of buffering against it, but it needs a real vblank anchor and NDL is
  submit-only); an **adaptive `PLANE_LEAD_MS`** on the offload route.

## ABR startup probe

**"Automatic" bitrate fires a capacity burst ~2 s into every session, and unbounded on Wi-Fi that
can cost the session its video entirely** — not a slow start but a flow that never establishes.
Measured on G5: a "successful" 2 Gbps probe still reported `send_dropped=20211`, i.e. the link
hammered far past what it can carry (~245 Mbps airlink ceiling). Capped, the same link reports
`send_dropped=0-167` and stream starts are reliable.

Don't read a slow *start* as this bug — a host compositor coming up has its own startup time. The
signal that matters is packet drops on the probe and video that never arrives at all.

**The cap is this client's.** `main.rs`'s `set_abr_env` sets `PUNKTFUNK_ABR_PROBE_KBPS` to 320 Mbps
before anything spawns a thread (`setenv` isn't thread-safe, and core reads it while building its
data-plane pump) — the connection test's proven-safe target, high enough to clear core's 70 %
margin and far below the mode-derived ~1.8 Gbps 4K120 target. `PUNKTFUNK_ABR_MAX_MBPS` clamps the
learned ceiling to the settings slider's 200 Mbps maximum. Host/network signals own the climbs and
descent; NDL contributes only measured feed backpressure.

Blind alleys, so they aren't re-tried:

- `bitrate_kbps == 0` (Automatic) arms **both** the AIMD controller and this probe — the client
  cannot separate them.
- `PUNKTFUNK_ABR_PROBE=0` disables the probe but leaves the climb ceiling at the negotiated start
  rate (~20 Mbps), which core's own comment calls a box "Automatic could NEVER climb out of".
- Running our own capped probe instead does **not** work: `request_probe` completes, but
  `abr.set_ceiling` is only called from core's own probe path, so the ceiling never moves. There is
  no public bitrate/ceiling setter on `NativeClient`.
- Pinning a fixed bitrate also disarms the probe, but costs mid-session adaptation entirely.

## Reconnect

A session that ends with `PunktfunkEndReason::Lost` (idle timeout, reset, network) is dialled again
up to `RECONNECT_ATTEMPTS` (3) times with the same target and settings, a toast up over the emptied
plane; the host lingers a dropped session for exactly this. Back, the EXIT gesture or a quit gives a
dial up. Any other end (game exited, host ended, host error, our own stop) goes to the menu. A
session that streamed a minute earns the budget back.

## Network speed test quirks

Burst is 320 Mbps / 3 s (not 3 Gbps / 5 s) — the UI thread shares a 3-core Cortex-A9, and an
unbounded firehose starves the app. 320 still detects any ceiling that would change the clamped
recommendation (>~285 Mbps); a 400 Mbps burst only raises the shed overshoot (51 % packet loss vs
38 % at 320). The probe must advertise `VIDEO_CAP_CHACHA20` like a real session (core's
`bytes_received` increments *after* AEAD decrypt). **~245 Mbps airlink ceiling** measured on G5
Wi-Fi (MediaTek USB 2.0 Hi-Speed bus), independently confirmed with a raw UDP flood — nothing client
code can raise. New flows sometimes black-hole 10-29 s (AP/driver setup), so
`session::probe::run_speed_probe` waits for the first completed video frame (cap 35 s) before
bursting — plane live, path warm.

## Video backend: NDL

NDL DirectMedia is the only backend. NDL has no decode context; calls go through the `NdlVideo::ffi`
mutex (the header says not thread-safe). AV1 remains disabled (never produced picture).

Backpressure: the video pump samples `render_buffer_length` every 500 ms; two samples of ≥ 8 frames
freeze the feed and ask for a keyframe, exactly the loss path (no flush — a flush restart kills the
audio plane). Measured sessions sit at 0–1, so this only fires on a real decoder stall.

An SMP (Starfish Media Pipeline) backend for webOS 3.5-4.x was built and removed (issue #164): never
verified on real 3.5-4.x hardware, and it carried a C++ shim `.so`, an ACB sink and a Settings row
for the whole NDL v1 audience. Those TVs get NDL v1 (H.264/SDR).

## NDL generations: v2 (webOS 5+) and v1 (3.5-4.x)

- Same library, two ABIs: v2 (`DirectMediaLoad`, `DirectVideoPlay`, `FlushRenderBuffer`,
  `GetRenderBufferLength`, `SetHDRInfo`) vs v1 (`DirectVideoOpen/SetCallback/SetArea/
  PlayWithCallback/Close`). webOS 4 has no v2 symbols.
- **Must `dlopen`, never link** `libNDL_directmedia.so.1`: a `DT_NEEDED` breaks webOS 4 startup
  under BIND_NOW (fails before `main`). Do not re-add `#[link(name = "NDL_directmedia")]`;
  `-Wl,-z,lazy` is not an acceptable workaround.
- Backend generation comes from `device::ndl_generation()` (`sdkVersion`): v1 for `<5`, v2 for `>=5`
  and unknown. The version selects what to try; `dlsym` is the final authority (no silent fallback).
- v1 limits: H.264 + SDR/BT.709 only; no input PTS, render-buffer query, flush, or HDR API.
  **Resolution is not capped here**: decode dimensions are passed through; `1920x1080` in
  `ndl/v1.rs` is only the display rect from `SetArea`.
- `NDL_DIRECTVIDEO_DATA_INFO_T` must include `source` (`width,height,source`). Omitting it fed stack
  garbage into `NDL_DirectVideoOpen`; now explicitly `NONE` (0).
- The M3/KADP runtime codec patch remains intentionally unused.
