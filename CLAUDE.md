# CLAUDE.md

Native LG webOS TV client for [punktfunk](https://git.unom.io/unom/punktfunk) — low-latency
desktop/game streaming. Targets webOS 5.x+, built on `punktfunk-core` (pinned git dep).
One target: Linux (webOS armv7 cross target, or a plain Linux box).

## Commands

[go-task](https://taskfile.dev), `task --list`. Bare targets run natively (how CI runs);
`docker:*` wraps the cross-toolchain, which is what you want locally.

| Task | What it does |
| --- | --- |
| `task docker:check` / `docker:build` | `cargo check` / release build |
| `task docker:lint` / `fmt` | clippy / `cargo fmt` |
| `task docker:test` | run the unit tests (the only task that RUNS them; `lint` only type-checks) |
| `task docker:package` | build + `dist/*.ipk` |
| `task docker:deploy` | run the app in a container over VNC — UI work needs no TV |
| `task deploy TELEMETRY=auto` | install to the TV, stream logs here (`TELEMETRY_LEVEL=debug\|info\|warn\|error`) |

CI lints with `-D warnings` and clippy is load-bearing — run `docker:lint`, not just `check`.
CI also runs `task test` on the host target; the cross build cannot execute tests.

## Architecture

Layered, deps point inward, acyclic:

`core` (pure domain: `Settings`, `Screen`, events, `caps`) ← `ui` (geometry, animation
clocks, the focus map, **no sdl2**) and `services` (portable I/O: store, discovery, mTLS
library, art, wol) ← `session` (streaming on `punktfunk-core`, **no sdl2**) and
`platform/webos` (the SDL2 and hardware boundary — input, NDL video, audio, evdev) ← `app`
(the `App` state machine and its painters) ← `runtime` (the two top-level loops).

- **Everything draws on the console kit** (`pf_console_ui`: `theme`, `widgets`, `icons`), on
  the shell's GL context (`console::gl`), immediate mode, once per frame. `app::draw::<screen>`
  is a painter with one `layout` that the pointer hit tests in `app::pointer` call too, so what
  is drawn is what is hit. Sizes are the kit's design units scaled by `Frame::k`; Home's grid
  and sidebar keep their pixel geometry in `app::view::{home,sidebar}` because the focus map
  navigates it. `runtime::overlay` draws what sits over a stream (stats, log tail, toast, the
  two confirm dialogs) the same way, over a transparent clear.
- **`app`** splits per screen by concern: `state::<screen>` (events, transitions),
  `view::<screen>` (copy and geometry) and `draw::<screen>` (the painter). `app::render` holds
  `prepare_grid` (the O(visible) cover window: art requests, keep-window eviction, the reveal
  wait) and `state` (grid, modal fade, hero, press, the kit list widget). `App` owns almost
  nothing directly: `nav` (screen, previous screen, one focus cursor per screen), `jobs`
  (every background receiver, one `drain_jobs`), `library` (the selected host's games, art and
  pins), `hosts` (the known list, reachability, rooted), `settings_ui`, `screens::slots`
  (per-screen payloads) and `render`. `screens::{list,confirm}` hold what a whole family of
  screens shares. Every field is `pub(crate)`; `runtime` writes through named setters.
- **`console`** hosts the shared gamepad shell (`pf-console-ui`) on the same GL context:
  `gl` (Skia over framebuffer 0), `model` (host rows, library, the command bus). Linux-only:
  the prebuilt Skia archive exists for armv7 (the TV) and aarch64 (`task test`, CI), so on
  macOS and Windows the module is absent and `runtime::console_flow` is a stub.
- **`runtime`** alternates two phases: a menu and `stream`, on `StreamOutcome::ReturnToMenu`
  vs `Quit`. The menu is one of two flows, picked per entry by `Settings::console_ui`:
  `ui_flow` (this client's own screens) or `console_flow` (the shared shell). Both reload the
  document on entry, which is what keeps the flip from showing one side's edits stale.

Rendering is immediate mode on Skia GL, redrawn on change and while anything animates.
Add a screen: a confirm is a `Confirm` in `app::screens::confirm` plus a title in
`app::draw::dialog::title_of`; a row list is a `ListCard` arm in `App::list_card` (rows as
`FocusRow`, mapped by `app::draw::list::row_spec`); anything else gets its own
`app::draw::<screen>` with a `layout` and joins `app::draw::ported`. The `app::screens` tables
are exhaustive over `Screen`, so the compiler asks.

## Invariants worth knowing before you edit

- **The grid is O(visible), not O(library)**, at every layer: covers are requested in a
  scroll window and evicted outside a hysteresis window (only when that window moves); the
  drawn range, the focus map and pointer hit-testing are all computed arithmetically rather
  than by scanning. A new path that walks `self.games` per frame, per keypress or per pointer
  motion is a regression.
- `focus_window` must always contain the current focus, or `FocusMap::navigate` finds no
  origin and focus silently freezes.
- **The kit's list widget mirrors `nav`'s cursor, never the reverse.** `App::kit_list_visual`
  feeds it the event for the look (recoil, dip, slip); the meaning is the App's handler.
- **NDL is `dlopen`'d, never linked** — a `DT_NEEDED` breaks webOS 4 startup before `main`.
- **`settings.json` is the shared schema, stored whole.** Settings are `pf_client_core::
  trust::Settings` (this TV's rows under `webos.*`, read through `core::settings::TvSettings`)
  and each host is `trust::KnownHost` flattened into `core::model::KnownHost` (plan D8). Never
  rebuild either from parts: a field this client does not model is another client's, and
  dropping it resets that client's row on the next save. There is no migration path — a
  document this build cannot read is replaced by defaults.
- **A test behind the arm gate never runs**: `task test` builds the host target, and an armv7
  test binary cannot execute on a runner. Real logic goes in `services::store::shared`
  (ungated, tested); only glue goes behind the gate.
- Video decodes through NDL DirectMedia (opaque decode+present, two generations picked by
  `device::ndl_generation()`); audio is client-side Opus.
  `core::caps` publishes the resulting limits and has three readers that must agree.

**Before any platform, perf or A/V work, read `docs/NOTES.md`** — soft-float, glibc shims, the
SDL fork, NDL's audio-plane pacing requirement, and a long list of measured blind alleys.
Debug real behaviour on the TV early; code-only theories about this hardware are usually wrong.

## Code comments

Only where necessary. Concise WHY comments — non-obvious invariants, platform workarounds,
subtle constraints. Never restate the code.
