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

`core` (pure domain: `Settings`, `Screen`, events, `caps`) ← `ui` (geometry, animation clocks, the
focus map, **no sdl2**) and `services` (portable I/O: store, discovery, mTLS, art, wol) ←
`session` (streaming on `punktfunk-core`, **no sdl2**) and `platform/webos` (the SDL2 and hardware
boundary — input, NDL video, audio, evdev) ← `app` (the `App` state machine and its painters) ←
`runtime` (the two top-level loops).

- **Everything draws on the console kit** (`pf_console_ui`), immediate mode per frame.
  `app::draw::<screen>` painter uses one `layout` for both hit-testing and render.
  Sizes scale by `Frame::k`; Home's grid/sidebar keep pixel geometry in `app::view::{home,sidebar}`.
  `runtime::overlay` draws stream overlays (stats, log, toast, dialogs) over transparent clear.
- **`app`** splits per screen: `state::<screen>`, `view::<screen>`, `draw::<screen>`.
  `app::render` holds `prepare_grid` and state. `App` owns `nav`, `jobs`, `library`, `hosts`,
  `settings_ui`, `screens::slots`, `render` (all `pub(crate)`, written via setters).
- **`console`** hosts the shared gamepad shell on the same GL context (Linux-only; Skia prebuilt
  for armv7/aarch64; macOS/Windows stub out `runtime::console_flow`).
- **`runtime`** alternates menu and stream on `StreamOutcome`. Menu is `ui_flow` or `console_flow`
  per `Settings::console_ui`; both reload settings on entry.

Add a screen: confirm = `app::screens::confirm` + title in `app::draw::dialog::title_of`.
Row list = `ListCard` arm in `App::list_card` + rows via `app::draw::list::row_spec`.
Other = own `app::draw::<screen>` with layout, joins `app::draw::ported`.
`app::screens` tables are exhaustive over `Screen`.

## Invariants worth knowing before you edit

- **The grid is O(visible), not O(library)**: covers, focus, hit-testing are arithmetic.
  Never walk `self.games` per frame or input.
- `focus_window` must always contain current focus, or focus silently freezes.
- **The kit's list widget mirrors `nav`'s cursor, never the reverse.**
  Visual feedback (recoil, dip, slip) is `App::kit_list_visual`; meaning is the handler.
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
