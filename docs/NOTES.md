# Architecture & platform gotchas

Verified against LG CX (webOS 5.6) and G5 (webOS 10.3). Load-bearing decisions only.

## Toolchain

- Cross target `armv7-unknown-linux-gnueabi` (tier-2) + webosbrew toolchain. Linux-aarch64-only; CI native, dev Docker.
- `.cargo/config.toml` wires linker to `scripts/cc-shim.sh` (passes `--sysroot` explicitly).
- **Soft-float was single biggest perf fix** (~300ms → ~30ms render). Non-`hf` target disables hardware FP codegen. Fix: `target-feature=+neon,+vfp3,-soft-float` + `target-cpu=cortex-a73` in `.cargo/config.toml`. Codegen only, not FFI ABI.
- **glibc shims required** (`src/platform/webos/glibc_compat_shim.c`): webOS glibc ~2.12 lacks `getauxval`/`gettid`/`sendmmsg`. Must land AFTER libstd via `cargo:rustc-link-arg` (single-pass linker drops `link-lib=static` early).
- **SDL3 must be webosbrew fork** (release-3.4.16-webos.2). Only fork has Wayland shell-integration, the webOS scancodes and `SDL_webOS*`; webOS ships no SDL3 at all. Fetched to its own prefix (`$SDL3_PREFIX`, NOT the NDK sysroot — the release says so, and buildroot's pkg-config wrapper mangles a .pc outside it), linked by path from `build.rs`, bundled as `lib/libSDL3.so.0` with `$ORIGIN/../lib` RPATH.
- **webOS scancodes moved in SDL3** — the block was compacted from 340-505 to 352-375. Home 384→364, Back 482→367, Red/Green/Yellow/Blue 486..489→370..373, Guide 495→374, Exit 505→375.
- **rust-sdl3's `Keycode`/`Scancode` are closed enums,** with no variant for any webOS key. Every webOS key comes back `None` on BOTH fields of `Event::KeyDown`, so none of them can be named from the safe event API. Two different answers, and the split is load-bearing:
  - **The colour keys and Back are matched on `Event::KeyDown`'s `raw`,** which carries the plain evdev code (`KEY_RED`..`KEY_BLUE` 0x18e-0x191, `KEY_PREVIOUS` 0x19c) — see `platform::webos::input::RemoteKey`. Polling the state array for these is a **dead end**: Back and Red never set a bit at all, and bits in that range latch (368/369 `CURSOR_SHOW`/`CURSOR_HIDE` read as held all session), so a wrong guess sticks on. Matching the event also restores real down/up edges, which a poll can only approximate.
  - **Only Home (364) and Exit (375) are polled** with `webos_scancode_down`: they do set state bits and have no event worth matching.
- **cmake/opus**: `punktfunk-core`'s `quic` feature needs CMAKE_POLICY_VERSION_MINIMUM=3.5 (modern CMake refuses vendored libopus's old minimum).
- **Shipped builds use fat LTO, one codegen unit** (`Cargo.toml`'s `[profile.release]`). Cross-crate inlining (AEAD, FEC, QUIC parsing) required for armv7 hot loops. Docker tasks default thin LTO/16 units for speed; override with `RELEASE_LTO=fat|thin|false`.
- **libstdc++ is linked statically, never bundled.** Bundled `lib/libstdc++.so.6` via `DT_RPATH` outranks `LD_LIBRARY_PATH`. webOS 11's `libNDL_media_impl.so.1` needs `GLIBCXX_3.4.32` (missing in SDK copy); webOS 10 needs 3.4.30. Static works on every firmware. No C++ crosses boundary — SDL, NDL, Luna are all C.

## Dependency resolution

`Cargo.lock` here controls shared core's dependencies too; Cargo ignores the upstream lockfile.
Build/check/lint/test, preview, and license metadata use `--locked` to preserve this resolution.

- Keep `quinn-proto` at 0.11.18 or newer: 0.11.17 double-subtracts evicted datagram bytes,
  causing `datagrams.outgoing.payload_bytes desynchronized` under send-buffer pressure.
- `flate2` 1.1.9 shares `miniz_oxide` 0.8 with PNG; 1.1.10 introduces a second version.
  Recheck both consumers when updating compression dependencies.
- Audit the shipping target with `cargo tree --locked --target armv7-unknown-linux-gnueabi -d`.
  Repeated identical versions can be separate host build dependencies. Core's SPAKE2 and
  session crypto require incompatible crypto/RNG generations; lockfile pins cannot unify them.

## UI preview (container)

`task docker:deploy` runs the app in a container on a virtual 1080p display, served over VNC (`http://localhost:6080/vnc.html`). UI work needs no TV.

- **Build uses `release` profile with Docker's thin LTO** (`scripts/preview.sh`). Host-native build shares `target/release` with cross builds; different profile causes rebuilds. `dev` profile reads as input lag on llvmpipe.
- **A punktfunk host runs in the same container** (`scripts/host.sh`, backgrounded; prints console URL when up). Client reaches it at `127.0.0.1`. Packages are amd64 under qemu binfmt on arm64; all tasks use `rust:trixie` (glibc 2.41) for shared cargo/target volumes.
- **No GPU and no mDNS.** llvmpipe means animation timing here is not the TV's, and Docker's "host"
  is the Linux VM, so `services::discovery` sees no LAN multicast — add hosts by hand. Unicast is
  unaffected, so a hand-entered host pairs and speed-tests for real.
- **Launch params reach app as argv[1] JSON**, just like SAM on TV. Set `WEBOS_SDK=4.0.0` to test NDL v1 path. Telemetry at `TELEMETRY_LEVEL`.

## UI rendering

Immediate mode on Skia over shell GL context (`console::gl`), drawn with console kit (`pf_console_ui`). Redraw on change/animate; stream loop: 33ms/500ms cadence for overlays, transparent clear for NDL plane.

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
- **Coalesce backdrop refreshes through widget motion**, not just the 75ms panel fade.
  Refresh dirty pages at most every 100ms (150ms during motion), so continuous animation
  cannot freeze the background. Retain the previous blur on capture failure, and warm
  scrolling lists too: their edge masks add a layer missing from short-list warmup.
- **The drawable can differ from the display mode** on webOS: every frame scales the canvas from
  layout-box units to `size_in_pixels`, and every layout and hit test works in layout units.
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
- ⚠ **The `ui_scale` launch param is deliberately untyped.** Tuning unowned panels on glass requires it. Type rejection silently kills all other params; `ares-launch` sends strings.
- ⚠ **Swap interval is vsync in menus, immediate over streams.** Menu loop needs blocking sleep; stream loop must not (same thread forwards input). Vsync adds ~16ms per UI action over stream. Pushed after `make_current` — interval is per SURFACE, shared by SDL renderer.

## Video decode (NDL DirectMedia)

- `libNDL_directmedia.so.1` is the real device library; the NDK sysroot ships a link-time stub.
- PTS = milliseconds since `NDL_DirectMediaLoad`, not wall-clock.
- Audio is decoded client-side via Opus unless offload is on — see *NDL's audio plane*.
- **`core::caps` has three readers that must agree**: `session::connect` (truth), `ui::settings` (offer), `Settings::clamp_to_caps`. Backend changes affect all three.
- **Decouple decode dimensions from punch-through rect** — else a 1080p stream on a 4K panel
  punches only the top-left quarter.
- **Loss recovery required** — no periodic IDRs in the stream. `session::pump::video_pump` calls
  `note_frame_index()` every frame (throttled RFI on gaps) plus a `request_keyframe()` backstop
  when `frames_dropped()` climbs.
- **Freeze-until-reanchor adapted for NDL**: NDL does decode+present in one opaque call (no split),
  so the client reimplements the skip-until-reanchor subset. A forward gap arms `holding`; frames
  are withheld until one arrives with `FLAG_SOF` (IDR) or a recovery anchor.
- ⚠ **Request keyframe only while SKIPPED, never before checking hold lift.** Resume frame restarts itself. Early check returned `NeedKeyframe` for already-fed AU, dropping pieces of resume keyframe on v2 (slice-progressive) — second freeze after recovery.
- **Count on `Presented`, not arrival.** Otherwise fps overlay ticks through freezes and refused plays count as frames.
- **Name hold only after 300ms** (`HOLD_TOAST_AFTER`, once per hold). RFI and startup probe loss clear inside round trip; rising-edge toast fires on every Wi-Fi session start.
- **Multi-slice stays opt-in** (`webos.multi_slice`, Settings ▸ Display ▸ TV). On, it advertises `VIDEO_CAP_MULTI_SLICE` so the host can emit slices while still encoding — overlapping encode and transport. The client still feeds NDL whole AUs only: `frame_parts` is always `false` and NDL's `partial_au` cap is `false`, so core reassembles one complete access unit and `NDL_DirectVideoPlay` runs once per picture. That removes the partial-AU copies/submissions and the unsafe broken-AU recovery (the required flush kills the audio plane) while keeping whatever host-side pipelining the slicing buys. Whole-AU feeding may avoid the corruption incomplete-AU feeding caused, but multi-slice decoder compatibility and bitrate efficiency stay device-dependent — no guarantee on smearing or compression artifacts.
- HDR mastering metadata can change mid-session — drain `next_hdr_meta` every frame.
- **`NDL_DirectVideoSetHDRInfo` forces panel to HDR on any call.** Ignores SDR; SDR/H.264 shows in HDR mode. No-op when `meta` is `None`; only real HDR metadata reaches NDL. Costs: no VUI fix, HDR gated to HEVC end-to-end.
- **Re-entering HDR per packet drops panel to 60Hz.** Applying on every host HDR packet caused 1440p120+ stutter; apply once and on change only.

## DualSense feedback: hidraw when wired, the Bluetooth service otherwise

**Wired pad gets `/dev/hidraw0`**, `root:jailer` read-write (verified non-rooted G5, webOS 10.3). No `/sys/class/hidraw` in jail; identify by `HIDIOCGRAWINFO`. Wired pad: 48-byte `0x02`, no CRC, one syscall/write; same 47-byte block as Bluetooth `0x31`, so all effects work. Bluetooth: needs `0x31` framing+CRC, so `hidraw.rs` claims `BUS_USB` only.

Adaptive triggers work on a **non-rooted** TV, but not through SDL. Verified end-to-end on G5
(dev-mode install, `DualSense` over Bluetooth): trigger resistance, section walls and lightbar
colour all confirmed on real hardware.

- **Bluetooth route:** `luna://com.webos.service.bluetooth2/hid/internal/sendData`. In `compat.api.json`'s `public` group; dev-mode app gets `["ares.webos.cli", "public"]`. Restricted `devices`/`bluetooth.manage` not needed.
- **Payload traps:** `reportData` must be int array, no `reportId` key. Extra property fails silently ("schema mismatch"). `setReport` fails (error 4); only `sendData` works. `getReport` hangs; needs deadline.
- **Report must be CRC-signed** per `hid-playstation`: 78 bytes (`0x31`, seq<<4, `0x10`, 47-byte block, 24 reserved, CRC32-LE over `0xA2` seed+body). Wrong CRC silently ignored; service returns `true` anyway. Don't prepend `0xA2`; stack adds HIDP header.
- LG backported `hid-playstation` to kernel 5.4, so the pad binds as three input devices
  (pad/motion/touchpad) sharing one `U: Uniq=` MAC — where `dualsense::find_address` reads it.
- **Rumble does not use either path**: the pad's event node advertises `EV_FF` and is
  group-writable by `compositor` (the app's uid is in it), so rumble goes through SDL's evdev force
  feedback when SDL advertises motor support. Bluetooth DualSense uses HIDAPI with enhanced
  reports disabled, so SDL rumble is unavailable there. Reports in `dualsense.rs` never set the
  compatible-vibration valid flag, so the paths can't fight.
- `hid/internal/*` is undocumented vendor surface — feature-detected, failing soft, never assumed.
- **Spawned sends must be throttled, or the video plane goes black.** The fallback route
  forks/execs `luna-send-pub` per send, copying the page tables of a process holding SDL, the
  decoder and its buffers. A Steam/Gamescope host *animates* the lightbar, so an unthrottled
  spawner produced a **black panel with the frame counter climbing, `dropped=0`, `backlog=0`**:
  decode kept running while the compositor never presented. `dualsense.rs` drops identical states
  and spaces the rest by `MIN_SEND_INTERVAL` (250 ms) on that route.
- **In-process LS2 works from app binary path only** (`platform::webos::ls2`). `LSRegister(<app id>)` succeeds at `…/applications/<appid>/bin/punktfunk-webos`, replies in 1-3ms; elsewhere: `Invalid permissions`. Hub keys on exe path. `LSRegisterApplicationService` refused everywhere. Both libs dlopened; refusal degrades to spawn (16ms vs 250ms).
- **Luna replies unreachable from ssh jail:** no `/dev/ptmx` (no `script`/`ssh -tt`); `luna-send-pub` needs tty. Probe from inside app or via LS2 binary copied over app binary (`rm` + `cp` — dir is group-writable). Same trick installs packages when `ares-install` fails: call `com.webos.appInstallService/dev/install` with `{"id":"com.ares.defaultName","ipkUrl":"/media/developer/temp/x.ipk","subscribe":true}`.

Host side: games emit triggers only for `DualSense`, so handshake pad kind decides. **Controller** row defaults `Automatic` — mirrors attached pad (`gamepad::detect_type`), not wire `GamepadPref::Auto` (host picks Xbox 360). Per-session resolution; doesn't write back (preference stays "match pad"). Test: `PUNKTFUNK_TEST_FEEDBACK` sends scripted effects without game.

## DualSense audio over Bluetooth: sniff mode is the whole problem

Pad speaker/coils take Opus/s8-PCM over HID output (reports `0x32`/`0x36`/`0x39`). Sounded choppy until `device/internal/stopSniff` called: TV keeps link in sniff, output leaves in bursts, audio starves. `stopSniff`/`startSniff` in `public` group. Coil masked it.

**Sniff batches input too.** Pad input reaches kernel in ~77.5ms bursts (G5: ~10/s), presses wait for burst, taps shorter than burst arrive together. `platform::webos::pad_link` holds Bluetooth joysticks out of sniff for stream, returns to TV policy at end.

**Different payloads:** `stopSniff` wants address; `startSniff` wants HCI parameters, refuses others with `errorCode 144` (schema error). Working shape:

```json
{"address":"…","minInterval":96,"maxInterval":124,"attempt":4,"timeout":1}
```

Intervals are 0.625ms slots bounding transmit waits (~77ms anchor). Slower interval shows lag in UI. Report refusal from causing loop: replies async, process-wide, undispatched ones surface ~11ms into next session.

Only sniff methods in `bluetooth2` API; no link-policy/QoS call (nothing persistent). One `stopSniff` doesn't hold; stack re-enters sniff within seconds. Keeper re-asserts every 250ms, immediately on >50ms gaps (motion ~2.5ms). Replies in 1-3ms. Measured: ~400 reports/s vs ~10 bursts.

**Feed pad at its clock, never faster.** One report per 10.667ms, tick lands on next future interval. Overrun tick fires again immediately (catch-up undeliverable). Measured 110 reports/s vs pad's 93.75 broke speech. Pre-fill uses 2 reports/tick; stop when ring depth=target (after pre-fill, depth is clock drift).

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

⚠ `Envelope::active()` checks frame ARRIVAL only, not content. Suppresses wire rumble. Title streaming silence mutes motors, gives nothing back.

Speaker lane declared for Bluetooth pad only (`find_address`): `0x36` report, Luna-only transport. USB pad has no `Uniq`. Verified G5: speech continuous, sustained tone rougher (Opus seams mask speech).

USB route in jail: **no `/dev/bus/usb`** (usbfs/libusb out), **`/dev/snd` rw** with app uid in `audio`. PulseAudio answers `pactl`. Wired pad's USB-audio card untested.

## Known platform limitations (don't retry)

- **Panel refresh rate cannot be set.** `webosbrew/SDL-webOS` is read-only (`SDL_webOSGetRefreshRate`); no webOS set API exists. It and `SDL_webOSGetPanelResolution` are read once on the SDL thread by `device::probe_panel` — the resolution feeds `native_mode` (stock SDL reports the app plane as 1080p on a 4K set), the rate is logged only, since Native deliberately still dials 60.
- **Magic Remote Back needs `SDL_WEBOS_ACCESS_POLICY_KEYS_BACK`** before window creation. Same for Home/Guide. Launcher ribbon needs `SDL_WEBOS_ACCESS_POLICY_RIBBON=false`.
- **Access-policy hints are all-or-nothing,** latched at window creation; cannot scope to stream only.
- **Held Back is the EXIT key (375); a short tap is ordinary Back.** Don't time it; webOS detects the long-press. Poll `WEBOS_EXIT_SCANCODE` (edge-detected), open dialog on rising edge. Short stays Esc/back-nav. Needs both `KEYS_EXIT` and `KEYS_BACK`, or gesture SIGTERMs app.
- **The gamepad shortcut is a 1s hold of L1+R1+Start+Select** (`runtime::input::DisconnectChord`), every client's escape chord. It opens disconnect and is also forwarded as real game input. Chord cleared on fire/unplug (dialog swallows events, unplugged pad sends no releases).
- **Hidden window gets no pointer input.** Keep mapped, fully transparent `RGBA(0,0,0,0)` so NDL shows through (not `.hide()`).
- **Two independent cursors** — webOS + host over network. Three levers (order matters): `EVIOCGRAB` on evdev (starves compositor), `SDL_webOSCursorVisibility`, `show_cursor`.
  - **Compositor repaints lazy both ways.** Visibility branches: visible synthesizes event, invisible marks only. Under grab no event arrives; arrow stuck on screen. `Cursor::flush` warps (center when captured, else position). Timer-based workarounds gone.
  - **`WEBOS_CURSOR_TIMEOUT=0`** disables compositor auto-hide.
  - **Don't guard `is_cursor_showing()`.** `SDL_ShowCursor` cached; repeat hide is no-op, query wrong while arrow displays.
  - Motion unscaled; damping only masked evdev jitter.
- **Absolute pointer bounded by panel; capture needs relative.** webOS pointer can't leave screen. Capture switches SDL to relative, warps to centre each motion (`wl_starfish_pointer_set_cursor_position`). `SDL_SetRelativeMouseMode` always returns 0.
- **Real HID mouse bypasses SDL** via `/dev/input/event*` (readable in jail). SDL motion from compositor (smoothed/resampled for remote), jitters in games. `platform::webos::evdev` reads direct. Constraints learned hard way:
  - **Keyboards grabbed both modes, mice only under Capture.** Ungrabbed reaches surface-manager, modifier+click warps to centre. Grabbing fixes it. TV pointer needed for desktop mode.
  - **Grab per node, not event type.** Combo nodes forward pointer too.
  - **Keyboard nodes `KEY_A`/`KEY_LEFTCTRL` minus denylist** (`LGE *`, etc.). Virtual remotes unnavigable if grabbed.
  - **SDL echo suppression differs:** Capture+HID drops all SDL pointer; Capture off drops motion in keyboard's window (clicks real).
  - **webOS 23+ pads type as remote keys** (arrows/OK/Back); no device name in Wayland key. Pad presses on own node, remote on `LGE M-RCU - Builtin [0]`. Stream reads ungrabbed, admits key within 250ms (`RemoteGate`).
  - **SDL relative mode off** — fork warps per motion, thousand compositor round-trips/s.
  - **Device filter `EV_REL` with `REL_X`/`REL_Y`**, not absolute. Test `ABS_X`/`ABS_Y` specifically (mice report stray axes).
  - **`EVIOCGRAB` scoped to `set_active`**, not reader lifetime. Kernel releases on fd close (panic safe). Flag gates `sink` call; grabbed/forwarded can't drift.
  - **Hot-plug rescans gated on `/dev/input` mtime.** Opening ~40ms, ~20 empty nodes; unconditional rescan stalls reader ~1s.
- **Colour buttons carry no scancode and no keycode** — matched on the key event's `raw` evdev code with Back (see the Toolchain note above). No access-policy hint exists for them, and none of them can be polled off the state array.
- **Keyboard Win is Home-class:** gated with remote Home under `KEYS_HOME`. Capture both, relaunch launcher via luna on remote-Home keycode to reach host.
- **Thread priority boosting removed — don't re-add.** Renicing needs `CAP_SYS_NICE` or `RLIMIT_NICE`; SAM jail grants neither. Poller cost 100ms-interval thread 5s during connect (busiest on 3-core). All threads same priority.
- **Don't toggle window show/hide while NDL composites.** Silently kills process (Wayland crash). Test visibility in isolation.

## Runtime gotchas (LG CX/G5)

- Apps install to `/media/developer/apps/usr/palm/applications/<appid>/` = `$HOME`.
- `luna-send` over raw ssh needs `ssh -tt` (real PTY) or output swallowed; `ares-install`/`ares-launch` work.
- **Black screen despite decode:** launch via real lifecycle (`luna-send .../launch`, SAM jailed uid). NDL punch-through composites for SAM foreground app only.
- No env vars in SAM launch; `params` in `applicationManager/launch` reach app as argv[1] JSON.
- SDL/Wayland may report `refresh_rate=0`; clamp to default. (SDL3 reports it as a float plus an exact numerator/denominator pair, so 59.94 is no longer rounded to 60.)
- **Game mode/ALLM rooted-only:** public bus denies `settingsservice`; routes via hbchannel root exec. Settings row shown on rooted TV only.

## ChaCha20 over AES-GCM

32-bit userland on ARMv8-A. RustCrypto `aes` has intrinsics only for `aarch64`; 32-bit ARM falls back. ChaCha20 (add/rotate/xor, no intrinsics) stays fast. Advertise `VIDEO_CAP_CHACHA20` unconditionally — only cipher here.

## Large library handling

- **Cover window is O(visible):** requests art within `CARD_PREFETCH_ROWS` of viewport, evicts outside `CARD_KEEP_ROWS` (hysteresis). Only on window move.
- **Cover art:** `ArtLoader` request/response (UI asks visible, forgets scrolled). Cached as encoded bytes (`$HOME/art-cache/`, write-then-rename). Failed decodes deleted.
- 365 titles: decoded drops from 365 to viewport (~5 cols).

## Audio: two routes, one pipeline (SDL is the default)

- **rust-sdl3 0.20 callback teardown needs a guard.** Its callback wrapper frees userdata before
  destroying the running stream. `AudioPlayer::drop` unregisters the get callback under SDL's
  stream lock first. Preserve this ordering until the binding fixes its destructor. The stream
  owns its device; never wrap its borrowed device ID in an owning `AudioDevice`.

**Audio processing** picks route (`core::model::AudioRoutePref`). Both built on `session::audio::AudioStage` pipeline: decodes/forwards to selected `core::media::AudioSink`. One pump drives both. Third route = one `AudioSink` impl.

| Route | Label | Path | Layouts |
| --- | --- | --- | --- |
| `Software` (default) | Software (SDL) | libopus here → SDL device, NDL's clock plane on its metronome | up to 7.1 |
| `NdlOpus` | Offload (NDL) | Opus decoded by the TV; 5.1 re-encoded into NDL's layout first | 2, 5.1 |

**Why software default:** NDL paces picture against fed audio plane; network-fed plane inherits arrival jitter (the stutter silent clock plane cured). Offload shorter, selectable for comparison; overlay names which ran (`Opus SW`/`HW`).

**Offload is the surround route.** NDL decodes stereo Opus + one 5.1 layout: standard coupling `(FL,FR)+(RL,RR)` mono FC/LFE (`AudioLayout::Standard`, `ndl::OPUS_51_LAYOUT`). Every session asks host; known hosts encode untouched. Legacy hosts answer `(FC,LFE)` coupled; client re-encodes into NDL layout. Some sets load+play nothing (no probe); stays choice not default.

**Surround through Opus plane only.** G5→Denon AVC-X3800H: SDL route plays s16le 2ch via PulseAudio `pcm_output`; PCM plane 6ch confirms+plays but AVR reports 2.0. aurora-tv dropped NDL PCM 5.1. Read AVR, not ears: Denon `OPINFINS ?` on telnet:23 digits per channel.

**Offload only under NDL v2.** v1 (webOS 4-) has no audio type; `audio_plane` false, route collapses to Software (row locked). Audio row layouts follow selected route; Offload offers stereo+5.1, not 7.1.

**No mixdown; layout row is preference.** `Settings::audio_channels` = "5.1 where possible"; `Negotiated::clamp` narrows by route (`AudioRoutePref::max_channels`). Route can't-carry layouts never encoded/sent/decoded; preference survives route change. Mismatch at `AudioStage::new` is error, not downmix.

- **Sound Out narrows nothing.** `NDL_DirectAudioSupportMultiChannel` = multi-channel PCM leaves set now. "Will play" only with Pass Through; describes NDL PCM, not SDL device. Gating breaks 5.1 to stereo on defaults. Client asks chosen layout, webOS folds. `ndl::log_audio_output` at connect for width>stereo.
- ⚠ **`NDL_DirectAudioSupportMultiChannel` has out-parameter:** code written through `int *isSupported` (0=unsupported, 1=no device, 2=no passthrough, 3=will play). Reading return value is wrong+UB on ARM EABI; callee writes through `r0`. `NDLMultiChannelPCMCallback` codes shifted down by one; not interchangeable.
- ⚠ **Probe optional NDL symbols after dlopen.** `RTLD_DEFAULT` finds nothing until `libNDL_directmedia` opened; probes run at startup before session. `ffi::optional_sym` forces `ffi::common()` first; without it TV loses 5.1 silently.
- ⚠ **NDL's `maxBitrate` unreliable** — SoC tier, not link limit. Don't cap stream; aurora-tv ignores it too.
- **Samples converted only where NDL needs layout.** libopus → f32 (SDL takes f32); offload forwards stereo untouched, re-encodes 5.1 only. No second buffer on SDL route.
- **Software route latency is buffering, not decode.** Software Opus = 5% core; only terms ring depth+device quantum.
  - **Prime overshoots; shed walks back.** Ring inspected once/callback (10.67ms period, 5ms steps — first serve target+0..15ms). `JitterPolicy` shed walks depth to target one crossfaded 5ms frame/time. Policy owns priming; cheaper than second-guessing state machine.
  - **Device quantum logged at open** (`SDL audio device:`). SDL may negotiate ≠512 frames; larger silently raises target (floored at callback+5ms). Read that line before latency conclusions.
- **SDL ring runs `punktfunk_core::audio::JitterPolicy`** (`JitterTuning::AAUDIO`, unmodified). De-jitter state machine (Linux/Windows/Android/Apple). Prime to adaptive target, grow on underrun only, walk drift down one crossfaded 5ms/time. `crossfade_drop` fades both (shed+trim).
  - ⚠ **Removed once, restored deliberately.** "~35ms floor" claim wrong (old 25/90ms preset, fixed prime also 25/90 — no save). Lost was resilience: adaptive floor+crossfaded shed (replaced by uncrossfaded 65ms click) on worst link. Don't delete without `audio playback (SDL device)` numbers.
  - **Preset is `AAUDIO`, not local copy.** Same tuning; old `deprime_after: 5` callbacks → `deprime_ms: 60` (callback count varies per device). AAudio rationale (raw callback, client buffer, Wi-Fi bunching) = TV exactly.
  - **A/V sync loop unwired.** `set_sync_target` never called (reproduces unsynchronised). Never steered; video reference biased low by unobservable NDL decode+panel.
  - **Read `target_ms` in debug line.** Adaptive floor current answer; says if set needed >25ms base. `sheds` vs `trims` separates "drift inaudible" from "link outran headroom".

**Blind alleys, so don't re-try:**
- ⚠ **NDL PCM plane built, measured, removed.** Third route decoded Opus → NDL's `NDL_AUDIO_TYPE_PCM`. Fed on arrival, plane depth (NDL paces picture on it) = network jitter function; field: intermittent lag. Paced ring helped, small latency win for can't-carry-7.1 route (interleave inferred, unverified). Not worth third path.
- **SDL's audio queue API can't carry de-jitter** — put/available/clear only. Pull callback stayed. In SDL3 that callback is `AudioCallback<f32>`: it pushes into an `AudioStream` rather than filling a slice SDL owns. ⚠ Its `requested` is a count of **f32 samples, not bytes** — SDL's own C callback is handed bytes, but rust-sdl3 divides by `size_of::<Channel>()` first. Dividing again serves a quarter of every callback, which is audible as noise, not as a dropout.
- **Don't put audio drain on main loop.** `AudioQueue` is `!Send`; audio cadence behind UI rasterizer.
- **Don't shrink `DEVICE_BUFFER_FRAMES` below 512.** Smaller quantum buys more wakeups, misses. SDL3 has no `samples` field on `AudioSpec` — it is the `SDL_AUDIO_DEVICE_SAMPLE_FRAMES` hint, set before the device opens.
- **Don't split `lock_ffi` per plane** without device evidence. No NDL entry is thread-safe; contention real but second guard guesses vendor internals.
- **Don't fold clock plane keep-alive into audio pump.** Cadence 20ms; pump parks 100ms empty. One thread = starved plane = the stutter it prevents.

## A/V sync

Host stamps `pts_ns` on audio datagrams; NDL does sync with plane audio (one timeline). Client `AvSync` estimator gone with jitter ring — no depth to move.

What matters:

- NDL is submit-only (`NDL_DirectVideoPlay` reports nothing about presentation), so glass time can
  only be estimated and the decode+panel constant after the render queue drains is not observable
  from the app at all.
- ⚠ **Use `frame.pts_ns`, never the paced value**, wherever a host-clock comparison is made. Both
  are in scope at the submit site with near-identical names; the paced one has been mapped into
  NDL's player clock by `session::timeline::Pacing`.
- ⚠ **NDL fails load asynchronously, never recovers.** CX: state `0x12` `errorCode 600`, then all `Play` returns -1, clock plane `AudioPlay` fails (pacer thread exits), NDL reports `UNLOADCOMPLETED`. No in-session reload; re-anchor does nothing. Client spun on failed feeds, frozen, QUIC healthy. `ndl::fatal()` latches, `VideoSink::is_dead` carries up, stream loop ends session. 600 provocation unknown.

## NDL's audio plane: why every load has one

⚠ **NDL paces picture only when load HAS audio plane PRIMED.** Video-only ignores PTS, presents at feed cadence vs 120Hz panel — "smooth 1080p, random above" stutter. CX: frames ~60ms ahead still `render_buffer_length` 0-1, stuttered; same+audio plane smooth.

**Plane doesn't need feeding** (CX, 2026-09-16). 136s stale plane — idle, no host frames — picture paced normally, margins better than metronome. Load prime matters (aurora: one Opus frame at `LoadMedia`, nothing after). One set; `ndl_plane_feed=continuous` restores metronome.

**Every V2 load asks for stereo audio plane;** what rides it is separate:

- **Software decode** (default) — `platform::webos::audio` decodes real audio→SDL; plane carries load prime only. `NdlVideo::run_clock_plane` watches (unconfirmed-plane check) unless launch param restores metronome.
- **Hardware Opus decode** (Audio processing→Offload, opt-in) — pump feeds real stream on video timeline; no SDL device opened.

`run_clock_plane` runs both (`session::pipeline::spawn_plane_threads`). Metronome retired, only job is unconfirmed-plane check. Dead-capture filler gone: starved plane doesn't freeze picture.

A set that refuses the audio plane outright ends up video-only at the load and gives up pacing with
it; the session log names which route it took. **NDL v1 has no Opus audio type at all**, so webOS 4
has no pacing reference.

Blind alleys, measured and ruled out: NDL's standing cushion depth (`render_buffer_length` is 0-1 in
smooth AND stuttery sessions), feed lateness (45-60 ms of slack either way), software Opus decode
cost (5% of a core, `dropped=0`), and HDR mode re-entry (a real bug, fixed in *Video decode* above,
but not this one).

### Wiring the plane

Byte-exact with `mariotaku/ss4s` `ndl/webos5`: `sample_rate` in kHz (`48.0` not `48000.0`), stereo `opus_empty_frame_211 = {0xec,0xff,0xfe}` prime, combined load. Struct layouts match `webosbrew/webos-userland` field-for-field inc. trailing `_padding` — memcpy'd into union arm, so implicit padding in `repr(C)` = uninitialized stack.

⚠ **NDL init lazy inside `load()`.** Warm init at startup causes "not loaded" on first load.

⚠ **`LOADCOMPLETED` not same thing every set, can't test plane before frame fed** (#188). 2025 QNED (webOS 10) reports it 26ms after first AU reaches decoder, never before: video-only misses 2s timeout, confirms when `ensure_loaded` gives up and feeds. CX ~40ms with no frame. Old code judged plane inside wait (pumps don't spawn until `session::connect` returns), QNED sessions read healthy plane as refused, fell back video-only, unpaced. That's #188 delay.

Plane **asked for, kept confirmed or not.** `AUDIO_PRIME_BUDGET` (500ms) buys fast CX confirmation only; unconfirmed load taken, metronome feeds (ingest-gated sets eventually callback). `run_clock_plane` **not gated on `LOADCOMPLETED`** — prime's continuation; gating leaves unfed from prime-end to first frame. QNED seconds, covers ~100ms ingest NDL standing cushion.

⚠ **No mid-session verdict, no in-session re-load.** One written for #188 reverted: couldn't be right (plane question resolution-independent, NDL `VideoInfo` no framerate; #188 4K120-HDR-only on set where 4K60/1440p120 fine). Re-apply HDR to new pipeline = mode-drop (fallback could break it). Rejected configs fail at load+fallback there — only fallback exists.

**Log line survives:** `v2::PLANE_CONFIRM_GRACE` (750ms past first frame) names unconfirmed plane. Window where two readings separate — before frame, "no callback" = healthy ingest-gated *or* rejected Opus config, accepts all frames to decoder that never runs. Nothing recovered deliberately: unmeasured set, guess costs #188. WARN in report = set to build fallback for.

Route picked from PROVEN plane (`AudioPlane::accepts_stream`), not asked-for: unconfirmed paces picture fine, but real audio can't ride maybe-never. Route can't re-pick mid-stream. Only real audio pays for answer: offload charges `AUDIO_PROVE_BUDGET` (2s), others short one (ingest-gated = pure black). Downgrade off offload worse than either (handshake clamped to stereo) — `audio path:` names it. Both waits bail early on `ndl::fatal()`. All losing-plane paths warn picture unpaced; grep log before theorizing delay.

Real audio feed gates on `LOADCOMPLETED` latch, not `feed_unblocked` (video gate, latches optimistic). Real audio on unconfirmed plane costs session sound; silence free — feeds gate differently.

Load blocks `session::connect` between handshake and first `next_frame`. Anything timing launch must cover it. `app::hero` fine (`FIRST_FRAME_WAIT` after connect, 30s max); `hdr_pattern` `PRESENT_DEADLINE` from `Playback::start`, must exceed sequence.

⚠ **Prime stamps and player clock share origin — `load_instant` is load CALL**, where NDL PTS starts. Used to stamp after wait, domains differed by D, every consumer corrected — offload real lead `PLANE_LEAD_MS − D` ≈ 0 on CX. One origin removes all. `last_real_feed_ms` seeded at construction, not 0.

⚠ **Metronome cushion `METRONOME_LEAD_MS` (80ms), not `PLANE_LEAD_MS` (40ms); one domain.** 80ms is depth 4K120 5.1 confirmed on (CX `plane_lead` 120 only vs lagging clock). Pinned there, not from TV load time. Under 80 = stutter risk; over = cheap (no lip sync). Knob for high-refresh stutter: walk UP vs `plane_lead`. Offload *fill* targets `PLANE_LEAD_MS` plus Smoothness extra (`set_plane_extra_lead_ms`), must match `play_audio` or real packets floor onto ceiling.

⚠ **Prime completes load.** Audio-enabled load doesn't report `LOADCOMPLETED` until plane received packet — but pumps don't spawn until `session::connect` returns. Deadlock = black picture, working sound, no error. `NdlVideo::prime_audio` feeds empty bursts through load window; CX turns "never" to `LOADCOMPLETED` in ~40ms. Prime's highest stamp seeds `last_audio_pts_ms`, first real packets floored not rewound.

⚠ **Never flush before LOADCOMPLETED — kills audio permanently.** Flush before done takes audio plane out; video recovers silent. `ensure_loaded` not-loaded error, sink holds+keyframe request without flush. Nothing queued. `NDL_DirectAudioPlay` returns 0 either way — no audio-side error.

⚠ **Hold must never respond to `NotLoadedYet` alone.** Freeze-until-reanchor skipped `play()` escape call, deadlock black first frame — worst static desktop. No new IDR.

⚠ **Never drain NDL queue with hold.** Emptied cushion breaks pacing whole session; trim stamps instead (#188).

⚠ **Audio stamps never backwards — NDL mutes rest of session.** Only feed points: `play_audio`, `burst_silence`. Both serialized under `lock_ffi`, both floor at `last_audio_pts_ms`. Read floor under guard or stale packet-ceiling race gives NDL stale stamp.

⚠ **Audio plane stamps off PLAYER clock, not host's.** Used to map host PTS through session clock, per-latch skew lifting resumed runs. **Ratchets:** freeze stalls timeline, packets arrive, resumed run below reached ceiling, only monotonic fix is add lead (unpayable). Field: CX offload 5 re-anchors, 78ms→124ms lead. Now stamped `player_clock + PLANE_LEAD_MS`, host PTS ignored: wall clock advances regardless freeze, `last_audio_pts_ms` absorbs reordering. Clock plane targets same, feeders share ceiling.

⚠ **Ratchet was real, not the mute.** Measured after change, saturated link (276 Mb/s competing vs 188 Mb/s stream): 32 re-anchors, `plane_lead` 37-40ms, stamps monotonic+even — audio died anyway. Don't re-try stamp arithmetic.

⚠ **Loss hold doesn't flush, THIS mutes plane** (confirmed device). Last ss4s diff (never flushes mid-stream — only unload+load recovery, keeps Opus plane). Every flush stops pipeline; used to follow with `PLAYING (0x1a)` transition. CX same storm: 16 re-anchors, 2s holds, not one `PLAYING` in log, `plane_lead` 38-40ms flat, audio intact vs flushing build killed it. `NDL_DirectVideoFlushRenderBuffer` safe+success; costs audio plane silently rest of session. Decode-error path flushes (actually errored); loss is network, NDL queue holds good frames hold about to present. `last_base_ns` survives pacing reset: no flush = pipeline holds pre-fed, restart from 0 walks stamp backwards.

Loss hold lifted by reanchor alone — waiting more leaves holds open.

This is where `mariotaku/ss4s` ended up too: `734e643` added thread feeding empty Opus through gaps, then `ef0c0ae` deleted it, moved both planes onto `CLOCK_MONOTONIC - mediaLoadedTime`. moonlight-tv#493 ("Stream loses audio after network hiccup") is unfixed version — same symptom, Opus only, PCM never reproduces, full restart needed.

⚠ Audio-enabled load returns success even when plays nothing — no probe distinguishes. Regression: `NDL load state:` log says if pipeline started. Out of hardware-decode: Audio processing→Software.

## Cadence pacing and the present cushion

Video feed copy-free — core reassembles contiguous `Vec` (NDL requires), sink passes pointer through; no Annex-B rewrite, no queue. Pacing = when bytes release.

`session::timeline::Pacing` wraps `punktfunk_core::phase::CadenceClock` (desktop/Android/Apple loop, all clients same stat): type-2 loop `ready − pts`, cushion `2 × MAD` (0.5ms floor, one-frame cap — core invariant). `snapping()` assumes sink latches panel grid (~half-refresh slack). **Unverified on NDL** — ss4s/aurora claim nothing, NDL scheduling undocumented. Read tuning as design assumption not behavior until measured.

Replaced mapping (fixed `base = player0 + (host_pts - host0)` + lead trim) gone. No rate term — free-run crystals walked lead away over minutes, 4ms jitter margin (below link spread). CX 1440p120: ~17% frames late vs loop's ~7%. "Stutters here, smooth on host monitor" report.

- ⚠ **Smooths offset, never timestamps.** Game at 45fps stays irregular (core `preserves_source_cadence`). Only transport removed.
- Can't fix: stream rate ≠ panel rate or divisor. 60 on 120 OK; 50 on 60 arithmetic.
- **Smoothness preference** (`PresentPriority`): **adds** 1-3 source periods on adaptive cushion. Needs timestamp clock+accepted v2 audio plane, else fallback+warning.
  ⚠ Used to SUBSTITUTE fixed for adaptive (no-op at ceiling — 120Hz >~4ms MAD = most). Additive now; every step worth period. Only way past core's one-period cap.
- ⚠ **Cushion ceiling is STREAM interval, not panel's.** Usually agree but different: bounds frame HOLD time, must follow host cadence. 120fps stream on 60Hz panel would license double-hold source can't justify.
- **Picture folds ONCE.** Slice-progressive repeats AU PTS across pieces at rising arrivals; per-piece mapping teaches loop tail arrival, inflates jitter by AU duration. `VideoStage::au_base_ns` holds stamp while AU open (every piece same timestamp for NDL). Folded at arrival (core wants it), estimate sees transport process. `snapping()` permanent (VRR needs live on-glass stamps, don't have). `note_off_cadence` wired; infer off-cadence from same-PTS AUs (compositor stamp, driver burst) — folding zero-interval teaches false gap. Returns `ready + cushion` (only advancing answer — anchored repeats, NDL truncates 1ms).
- **Re-anchor triggers:** freeze-until-reanchor hold, via `reset_timeline`. Source interval snapshotted at build; if mid-session mode change becomes path, fix snapshot.
- **Stamp sequence clamped monotonic per run** (`last_base_ns`): cushion shrinks between frames, NDL mutes on rewind. Only invariant whose break costs audio.
- `late_stamps` (frames stamped behind player clock, judder) reported as `pacing:` heartbeat/`Pace` overlay.
- **A/V offset = `plane_lead − cushion`** (`av=` heartbeat, `av ±Nms` overlay). Video pump holds both; both timestamp NDL's `elapsed_ns` (legal). Audio fixed `PLANE_LEAD_MS` ahead, picture mapped cushion ahead. Positive = sound behind. ⚠ Only where real audio (software plane = silent metronome, fiction). ⚠ Stamp domain: NDL decode+panel unobservable, bias picture later, true offset smaller. Trend/sign, never calibration. ⚠ Smoothness doesn't move it: same budget handed to plane (`set_plane_extra_lead_ms`), both shift, stays `PLANE_LEAD_MS − cushion`. Unmatched walks through zero (Smooth 2/60Hz = 33ms vs 40ms lead); sound ahead is more audible.
- ⚠ **No diagnostic takes NDL FFI lock.** `render_buffer_length` behind same guard as `video_play`; query stalls next feed. Backpressure path samples depth every `BACKLOG_SAMPLE` (500ms); `backlog=` reads that sample, not own query. Figure up to one interval old, stale during hold (sampling suspended) — `holding` says so. Other per-frame (`feed_us`, `late_submit`, `min_slack`) gated on `timed`; only ungated clock read is pacing input.
- ⚠ **Live counters on overlay, not log.** Periodic dump buries events (holds, refusals, slow feeds). `pacing:`/`video:` at TRACE (below usual `debug`). Overlay has all: cushion/jitter/late/slack, av stamp, plane_lead, backlog. Raise to TRACE offline only.
- **`cushion` only figure moving with presentation setting.** `jitter` = measured residual (independent), `late` = cumulative from start. Watching either shows nothing (inert-looking setting).
- **`min_slack` = complete-AU deadline margin** (`Pacing::note_submitted`, minimum not mean, one bad frame visible; taken/re-armed on print line — re-arm IS take, read elsewhere shortens window). Loop folds AU FIRST piece only (deliberate, tail arrival re-map), never sees slice-progressive COMPLETED. Negative `min_slack` while healthy `jitter` = large AU vs first-piece deadline. Slice-progressive >~25 Mb/s (core past FEC block ≈22KB), keyframes split, reachable ordinary. Read vs `parts=` video line: `parts=0` = inert. ⚠ Populated only while timed (`report_decode_latency || diagnostics`), diagnostic not steering — loop built on it silently open-loop.
- ⚠ **Stamp ceiled to whole ms in BOTH intents.** Rounding used to live in Smoothness branch. NDL truncates, rounding down spends cushion; generalizing is sub-ms behavior change to Lowest latency via Smoothness fix.

**Slice-progressive feed stays off** (`frame_parts` never requested, `partial_au` false). Core's `FramePart` contract is only for decoders with a `PARTIAL_FRAME` capability, and a broken AU must flush — NDL has neither: it takes raw Annex-B and finds boundaries by start code (no partial-frame flag, v1 can't even repeat a timestamp across pieces), and mid-stream flush kills the audio plane permanently. Host multi-slice transport pipelining (`VIDEO_CAP_MULTI_SLICE`) is separate and still offered: core reassembles one complete AU and NDL gets one play call per picture. `session::stage::parts` still implements the contract for a future sink that can honour it; the `partial_au` cap is what gates it.

⚠ **Real audio on plane must carry lead or PICTURE stutters.** Plane queue depth paces video; offload = real packets only, wire-fed ≈ player clock, depth ≈ 0, renderer edge-underrun, stutter on jitter. Fixed by `PLANE_LEAD_MS` (40ms) added to every stamp in `play_audio`; clock plane targets same, neither pushes ceiling. NDL no depth arg; stamp-future only way. Cost: lip sync `PLANE_LEAD_MS` behind. Walk down vs `lead` overlay audio line/`plane_lead=` heartbeat (only observable places).

- Unknowns: NDL plane depth (not `render_buffer_length`, no query), offload vs software where offload works. Both named overlay (`Opus SW`/`HW`) and log for reporting.
- Not tried: **phase-locked capture** (core has protocol — `report_phase`+`CLIENT_CAP_PHASE_LOCK` — host aligns capture to panel grid, reduces latency not buffer, needs vblank anchor; NDL submit-only), **adaptive `PLANE_LEAD_MS`** offload.

## ABR startup probe

**"Automatic" fires capacity burst ~2s into session, unbounded on Wi-Fi can cost video entirely** — not slow start, flow never establishes. G5: "successful" 2Gbps probe = `send_dropped=20211` (link hammered past ~245 Mb/s airlink ceiling). Capped, same link `send_dropped=0-167`, reliable starts.

Don't read slow start as bug — host compositor has own startup. Matters: probe packet drops+no video.

**Cap is client's.** `main.rs` `set_abr_env` sets `PUNKTFUNK_ABR_PROBE_KBPS` to 320Mbps before threads (`setenv` thread-unsafe, core reads while building pump) — connection test proven-safe, clears core 70% margin, far below ~1.8Gbps 4K120 target. `PUNKTFUNK_ABR_MAX_MBPS` clamps learned ceiling to slider's 200Mbps. Host/network owns climbs; NDL contributes measured backpressure.

Blind alleys:

- `bitrate_kbps == 0` (Automatic) arms both AIMD+probe — can't separate.
- `PUNKTFUNK_ABR_PROBE=0` disables probe, leaves ceiling at negotiated start (~20 Mbps) — "Automatic could NEVER climb out" (core comment).
- Own capped probe doesn't work: `request_probe` completes but `set_ceiling` only from core probe path, ceiling never moves. No public setter on `NativeClient`.
- Fixed bitrate disarms probe, costs mid-session adaptation.

## Reconnect

Session ending `PunktfunkEndReason::Lost` (idle timeout, reset, network) re-dials up to `RECONNECT_ATTEMPTS` (3) with same target/settings, toast over emptied plane; host lingers for this. Back/EXIT/quit dials. Other ends (game exit, host end, error, stop) → menu. Session ≥1 minute earns budget back.

## Network speed test quirks

Burst 320 Mbps / 3s (not 3 Gbps / 5s) — 3-core A9 UI starves on unbounded. 320 detects ceiling changes (>~285 Mbps for recommendation); 400 only raises overshoot (51% drop vs 38%). Probe must advertise `VIDEO_CAP_CHACHA20` (core's `bytes_received` after AEAD). **~245 Mbps airlink ceiling** G5 Wi-Fi (MediaTek USB 2.0), UDP flood confirmed — client can't raise. Flows black-hole 10-29s (AP/driver), `run_speed_probe` waits first completed frame (35s cap) before burst — plane live, path warm.

## Video backend: NDL

NDL DirectMedia only backend. No decode context; calls through `NdlVideo::ffi` mutex (not thread-safe per header). AV1 disabled.

Backpressure: pump samples `render_buffer_length` every 500ms; two ≥8-frame samples freeze+keyframe request (loss path, no flush — kills audio). Measured 0-1, fires decoder stall only.

SMP (Starfish Media Pipeline) for webOS 3.5-4.x built+removed (#164): never verified real hardware, C++ shim, ACB sink, Settings row for NDL v1. Those TVs get NDL v1 (H.264/SDR).

## NDL generations: v2 (webOS 5+) and v1 (3.5-4.x)

- Same lib, two ABIs: v2 (`DirectMediaLoad`, `Play`, `Flush`, `GetRenderBufferLength`, `SetHDRInfo`) vs v1 (`Open/SetCallback/SetArea/PlayWithCallback/Close`). webOS 4 has no v2.
- **Must dlopen, never link** `libNDL_directmedia.so.1`: `DT_NEEDED` breaks webOS 4 startup (BIND_NOW fails before `main`). Don't re-add `#[link]`; `-Wl,-z,lazy` not acceptable.
- Generation from `device::ndl_generation()` (`sdkVersion`): v1 <5, v2 ≥5/unknown. Version selects what to try; `dlsym` final authority (no fallback).
- v1 limits: H.264+SDR/BT.709 only; no PTS, buffer query, flush, HDR API. Resolution uncapped (pass-through); `1920x1080` in v1.rs = `SetArea` display rect only.
- `NDL_DIRECTVIDEO_DATA_INFO_T` must include `source` (`width,height,source`). Omit fed stack garbage; now explicit `NONE` (0).
- M3/KADP runtime codec patch intentionally unused.
