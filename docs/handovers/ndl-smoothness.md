# NDL smoothness: implementation, review and hardware validation

**Original request:** Explain why this webOS client can stutter more than Aurora on the same punktfunk host, then reuse the shared 1–3-frame smoothness preference for NDL while preserving existing presentation and documenting expectations.
**Branch:** `feat/ndl-smoothness` — **Status:** ready for hardware validation.
**Updated:** 2026-09-13. This is the consolidated implementation document and handover.

## Scope
- User's target: LG G5, 2560×1440 at 120 fps, HDR, 150 Mbps. The G5 stutter cause remains unidentified.
- Compared `../../misc/aurora-tv`, its bundled ss4s, native Linux `../../misc/punktfunk`, and the production dependency pinned at `a7cd5ff8e`.
- Aurora uses GameStream/Limelight; this client uses NativeClient. Same host does not establish identical transport or encoder settings.
- Preserve default presentation, progressive feeding and the previously successful silent audio plane. No deployment or hardware measurements were performed.

## Done
- Exposed shared **Prioritize** and **Smoothness buffer** controls in `src/app/state/settingspage.rs`, including profile overrides and reset-to-global behavior. Buffer appears only under Smoothness; changes apply next stream.
- Honored caption requests: priority note is “Smoother video, more delay”; buffer has no note. Removed redundant `spec.note = None`; the kit already supplies no note. Its separate `detail()` help was not changed.
- Routed resolved `present_priority` through `src/runtime/mod.rs`, `src/session/connect.rs`, `src/session/pipeline.rs` and `SinkConfig` into `src/session/timeline.rs`.
- Lowest latency retains the existing adaptive CadenceClock mapping. Smoothness substitutes a fixed selected cushion while keeping the same offset/drift correction.
- All compressed AU parts still reach NDL promptly, sharing one timestamp per AU. No new frame queue, copying, sleep, thread or per-frame allocation was added.
- Added final-AU lateness accounting in `src/session/stage/mod.rs` and `late_submit` in the DEBUG pacing heartbeat in `src/session/pump.rs`. INFO `video presentation` reports effective priority and negotiated rate.
- Removed all 10 tests added during this work at the user's explicit request, including their fixtures. Kept the 56 pre-existing tests; do not restore removed tests without a new request.
- Consolidated `docs/NDL-SMOOTHNESS.md` into this handover and updated the link in `docs/NOTES.md`.

## Presentation policy
- The shared resolver maps Automatic (`smooth_buffer = 0`) and invalid stored values to two frames. Pacing consumes the resolved enum; a debug assertion checks 1–3 without duplicating normalization. Release builds still rely on the resolver.
- Selected frames are the TOTAL video cushion, not N extra frames added to Aurora's offset or to the adaptive cushion.
- At 120 fps: 1/2/3 frames target about 8.33/16.67/25 ms. At 60 fps: 16.67/33.33/50 ms. Automatic selects two. Conversion uses negotiated source rate, not resolution, bitrate or panel rate.
- Schedule is approximately `host PTS + corrected clock offset + cushion`, not `arrival + cushion`. Late arrivals spend reserved headroom; their delay does not become an equal shift of the schedule. Source rendering irregularity remains visible.
- Implementation substitutes `due - clock.cushion_ns() + selected_cushion`. Core's due calculation includes exactly that adaptive cushion, including the repeated-PTS path; this does not double-count buffering.
- Smoothness alone rounds up to integer milliseconds, adding less than 1 ms to its target. Lowest latency keeps existing truncation, even at its adaptive 0.5 ms floor. Rounding is a small scheduling bias, not a late-frame guarantee.
- Monotonic timestamps survive loss recovery because NDL can interpret backward stamps as rewind and mute output. Repeated host stamps do not train the drift estimator. The offset absorbs differing host/player epochs.
- Default snapping tuning assumes roughly half-refresh panel-latch slack; its actual firmware behavior on the G5 is unverified. Core tests documenting the invariants are `preserves_source_cadence` and `cushion_respects_ceiling`.
- Linux owns decoded frames and implements FIFO/preroll. NDL owns decode/display here; a compressed-input FIFO would deny decode overlap, and dropping arbitrary compressed frames could break references.
- Headroom is not a hard queue-capacity or end-to-end latency bound. It cannot fix packet loss, sustained overload, source stalls or firmware ignoring timestamps.

## Audio, recovery and locking
- Smoothness requires a timestamp clock and an accepted audio plane; otherwise the stage logs fallback. Accepted-but-unconfirmed planes remain eligible, preserving the existing device readiness workaround.
- The UI's `audio_plane` capability gate is a proxy: only NDL v2 has that plane today, and it also supplies timestamped video. NDL v1 exposes neither control.
- Keep silent Opus's established 80 ms target with real audio through SDL. NOTES records video-only stutter despite ample timestamp slack, cured by feeding an audio plane. This is evidence for retaining the fix, not a universal model of LG firmware.
- Real audio timing is unchanged. Extra video headroom can affect lip sync; this implementation does not compensate audio delay automatically.
- Keep the global FFI guard. ss4s's unlocked video feed does not prove all LG firmware supports concurrent calls. Silent refill normally feeds four 5 ms packets every 20 ms; catch-up is bounded by its target. Contention is not measured here.
- Loss recovery resets the estimator without flushing and preserves timestamp monotonicity. Existing decoder-error recovery can still flush; no new flush/reload was added.
- Render depth is not calibrated presentation headroom or decode duration. CX observations of 0–1 at ~60 ms timestamp lead do not prove zero interaction on every paced firmware path. ABR continues using timed feed calls as a pressure proxy.
- Backlog recovery still requires two deep samples at least 500 ms apart. A sustained increase is normally detected after about 0.5–1 s, with delayed/failed queries potentially longer; samples do not prove a continuous one-second stall.

## Verified Aurora/ss4s findings
- Aurora defaults Smooth presentation OFF (`src/app/app_settings.c`), overriding ss4s's default; its NDL backend then supplies elapsed wall-clock timestamps.
- When enabled, `src/app/stream/session_worker.c` selects host-PTS-only pacing and `(75000000 + x100/2) / x100` microseconds, clamped 2000–12000: 12 ms at 60 fps, 6.25 ms at 120. It explicitly bypasses the synthetic interval grid.
- Aurora uses a fixed initial host/player anchor without ongoing clock-skew correction. Its configured offset is not guaranteed remaining headroom on every frame. Actual selected backends and toggle on the user's G5 are still unknown.
- Aurora feeds complete AUs. This client can feed completed contiguous FEC blocks, which are not necessarily codec-slice boundaries; `parts=0` means progressive delivery was inactive for that run.
- Aurora's NDL and Starfish audio backends advertise six channels. 5.1 is implemented; `src/app/stream/session.c` clamps requested 7.1 to 5.1. Generic 7.1 labels/decoding do not establish backend support.
- NDL 5.1 prefers client Opus→PCM when `NDL_DirectAudioRegisterCallback` is present (webOS 7+ heuristic), otherwise NDL Opus with layout handling. Discrete output to speakers was not verified.

## Diagnostics and review conclusions
- `late_stamp` covers first-piece mapping. `late_submit` compares completed feed against integer-ms PTS, including tail/FFI time but not asynchronous decode/display. Blocking feed can report lateness after successful presentation; neither counter is a dropped-frame count.
- More cushion reduces late-submit exposure for identical feed timings, but either mode can report nonzero. Zero does not prove smooth display.
- `cushion` is adaptive under Lowest latency and fixed under Smoothness. The unused adaptive value is approximately recoverable from logged jitter as `clamp(2 × jitter, 0.5 ms, source period)`, subject to log rounding.
- `plane_lead` is last submitted audio PTS minus elapsed application time, not measured buffered duration. These are application clocks, not hardware clock readbacks.
- External review correctly identified redundant note clearing, conflicting clamp policy and lost WHY comments; these are fixed. The later debug assertion enforces the resolver contract in development.
- Rejected overclaims: rounding has no established “6% of frames” effect; upward rounding moves the target later. Monotone truncation cannot turn a future stamp into an earlier integer-ms value at the same sample, but can make it equal, and later feed delays still matter.

## Validation and gotchas
- After external-review simplifications: Linux arm64 Docker `cargo test --locked --bins` passed all 56 retained tests; `cargo clippy --locked --all-targets -- -D warnings` passed. The final debug assertion was separately compiled/linted successfully. Formatting and diff checks passed.
- No armv7 cross-build or G5 validation yet. Local macOS tests fail in pinned pf-client-core's unsupported target configuration; use Linux Docker, not dependency changes to accommodate macOS.
- Docker: `rust:bookworm`, linux/arm64, existing punktfunk-webos cargo/rustup/target/apt volumes (mounts in `taskfiles/toolchain.yml`); install cmake, pkg-config, libsdl2-dev, libfontconfig-dev, libfreetype-dev; set `CMAKE_POLICY_VERSION_MINIMUM=3.5`.
- Skia HTTP 416 workaround: `SKIA_BINARIES_URL=https://github.com/rust-skia/skia-binaries/releases/download/0.99.0/skia-binaries-{key}.tar.gz?ndl-smoothness=2`. Do not force binary download: it attempted unavailable git metadata. Avoid accidental full Skia builds.
- Historical dead ends: “Aurora always has more cushion” was false. NDL PCM was already built/measured/removed: a paced ring fixed arrival jitter, but benefit was small, no 7.1 and unverified 5.1 ordering. Read NOTES before retrying.

## Left
1. Build/package armv7 using the existing task workflow before installation; review the branch before merging.
2. Capture Aurora's actual backend/toggle and both clients' negotiated codec/mode/bitrate on the G5.
3. Compare Lowest latency versus Smoothness 1/2/3 at 1440p120 HDR 150 Mbps, holding scene, network, audio route and progressive feeding fixed. Record logs, input latency and lip sync; verify displayed cadence with camera/display evidence.
4. Prefer the smallest budget that addresses measured jitter. Investigate locks, audio routing or further timing changes only when measurements identify their contribution.
