# CLAUDE.md

Native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) — low-latency
desktop/game streaming. Targets webOS 5.x+ (NDL v1 fallback for 3.5-4.x), built on
`punktfunk-core` (pinned git rev). One build target: Linux (webOS armv7 cross, or a plain Linux box).

## Commands

[go-task](https://taskfile.dev), `task --list`. Bare targets run natively (CI);
`docker:*` wraps the cross-toolchain (local dev).

| Task | What it does |
| --- | --- |
| `task docker:check` / `docker:build` | `cargo check` / release build |
| `task docker:lint` / `fmt` | clippy / `cargo fmt` |
| `task docker:test` | run the unit tests (the only task that RUNS them; `lint` only type-checks) |
| `task docker:package` | build + `dist/*.ipk` |
| `task docker:deploy` | run the app plus a punktfunk host in one container, over VNC — UI work needs no TV |
| `task deploy TELEMETRY=auto` | install to the TV, stream logs here (`TELEMETRY_LEVEL=debug\|info\|warn\|error`) |

CI lints with `-D warnings` and clippy is load-bearing — run `docker:lint`, not just `check`.
A host `cargo check` proves nothing: `app`/`platform` are cfg-gated out on macOS.

## Architecture

Layered, deps point inward, acyclic:

`core` (pure domain: `Settings`, events, `caps`) ← `services` (portable I/O: store, discovery,
mTLS, cover cache, wol) ← `session` (streaming on `punktfunk-core`, **no sdl3**) and
`platform/webos` (the SDL3 and hardware boundary — input, NDL video, audio, evdev) ← `console`
(the shared shell's GL host and service) ← `runtime` (the menu and stream loops).

- **The only UI is punktfunk's shared controller shell** (`pf_console_ui`). Screens, rows and
  navigation live in the kit; this client adds none. `console::model::Service` answers what the
  shell asks the binary to do (pairing, library, art, wake, speed test, log upload).
- **`console`** hosts the shell on a GL context on the app's window (Linux-only; Skia prebuilt
  for armv7/aarch64; macOS/Windows stub out `runtime`).
- **`runtime`** alternates `console_flow` (menu) and `stream` on `StreamOutcome`. The menu
  reloads settings on entry. `runtime::overlay` draws the stream overlays (stats, log, toast,
  stop dialog) on the same context over a transparent clear.

## Invariants worth knowing before you edit

- **NDL is `dlopen`'d, never linked** — a `DT_NEEDED` breaks webOS 4 startup before `main`.
- **`settings.json` is the shared schema, stored whole.** TV settings = `pf_client_core::trust::
  Settings` (webos.* rows via `core::settings::TvSettings`); hosts = `trust::KnownHost` flattened
  to `core::model::KnownHost`. Never rebuild from parts — unmapped fields belong to other clients.
  Dropping a field resets that client's row. No migration: unreadable docs → defaults.
- **Gated tests never run** (`task test` builds host only; armv7 can't run on CI).
  Real logic in `services::store::shared` (ungated, tested); glue behind the gate.
- Video: NDL DirectMedia (opaque decode+present, two generations via `device::ndl_generation()`).
  Audio: client-side Opus, or offload. `core::caps` publishes limits; three readers must align.

**Before platform, perf, or A/V work, read `docs/NOTES.md`** — soft-float, glibc shims, SDL fork,
NDL audio pacing, measured blind alleys. Debug on the TV early; code theories about this hardware
are usually wrong.
