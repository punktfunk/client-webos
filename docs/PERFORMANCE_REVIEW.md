# Performance and concurrency review

Reviewed revision: `38be23c`. Date: 2026-09-13.
Reviewer feedback rechecked on 2026-09-14 against the working tree, including its staged
ureq update from 3.4.0 to 3.4.2. Implementation started from baseline `aa97200`; original source line references
are historical and may have moved. Rollback patches record the exact changes.

Repository-wide static review, split across four reviewers. Areas covered: UI and
geometry, rendering and GL integration, audio and controller feedback, streaming
and NDL, input, services, persistence, logging, and build configuration. Hardware
constraints in `NOTES.md` informed the review.

The original analysis below describes pre-change behavior, not measured speedups.
Each section now starts with implementation status; the historical analysis is
retained for review and rollback context. No benchmarks, TV runs, or test suite
were run. Newly added tests were removed following the user instruction. Upstream
`pf_console_ui`, `punktfunk-core`, Skia, and vendor implementations were not
comprehensively audited. The installed, pinned ureq source was checked for its
timeout defaults. This is a hot-path and lifecycle review, not a claim that every
dependency or every possible execution path has been verified.

Priority: P1 should precede optimization work; P2 is actionable behavior or
avoidable work; P3 needs profiling or has lower expected impact.

The expanded analysis below separates source-proven behavior from its possible
hardware consequences. Reproduction sequences and acceptance criteria are
proposed tests, not tests already executed. Source locations refer to the reviewed
revision; concurrent dependency edits in the workspace are outside this report.

## Reviewer feedback verification

The reviewer correctly identified several scope and priority problems. The
following conclusions incorporate direct reinspection of the current sources:

| Point | Verified conclusion |
| --- | --- |
| Feedback idle stall (#9) | Only in-process LS2 without a coil envelope lacks a wakeup deadline. USB and subprocess transport are unaffected by this mechanism. |
| Feedback shutdown (#9) | Pending release can be lost on LS2 with or without a coil envelope. Contrary to the reviewer, this mechanism is not independent of transport configuration. |
| Telemetry (#13) | Opt-in launch-parameter hazard, primarily developer diagnostics. Normal launches use a file. Lowered to P3. Installed binaries can still take this path when explicitly launched with telemetry. |
| ureq (#12) | HEAD contains 3.4.0; the updated working lockfile contains 3.4.2. Verified 3.4.2 defaults directly. Relevant deadlines remain absent; `await_100` alone defaults to one second. |
| External connection reuse (#12) | Removed as a claimed benefit of this finding. Missing deadlines are the actionable issue; pooling benefits require workload evidence. Different URLs alone do not rule out reuse on the same origin. |
| Settings writes (#16) | A later successful whole-document save repairs earlier unsaved changes that remain in that document. Lowered to P3. An identical re-save demonstrates suppressed retry but is not required for loss at restart. |
| Console row cadence | Reviewer overlooked `SERVICE_EVERY = 100 ms` and the early return inside `Service::tick`. Periodic row rebuilds occur at most about ten times per second, plus event-driven rebuilds, not sixty. |
| Idle rendering | Equal 16 ms constants do not mean equal scheduling. Idle sleep is unconditional before drawing; the frame-budget sleep fills only the remaining budget. Idle mitigation is real but does not stop redraws. |
| Speculative candidates | Lock granularity, Bluetooth allocation, and management reuse are deferred cleanup, not established performance problems. Callback preallocation is suitable for a small opportunistic change with a justified capacity. |

No runtime reproduction or profiling was added during this verification.

## Implementation review follow-up (2026-09-14)

Reviewed the supplied code-review findings against the working tree. No new tests
were added or executed, preserving the explicit user instruction. The reviewer's
reported 59 passing tests are external evidence, not validation performed here.

| Review item | Decision and resulting status |
| --- | --- |
| B1 | Fixed: removed arrival-membership filtering. Live request tokens remain the eviction authority, allowing in-flight covers to survive regrouping. |
| B2 | Fixed: reports read the live input gate. A deactivation generation also preserves release edges across a quick close/reopen during poll. |
| B3, M2 | Fixed: identical console settings reach the writer; revision changes only on mutation. Snapshot documentation moved back to snapshot. |
| B4 | Fixed with a different mechanism: cancellation and media entry synchronize on the attempt mutex. Network-only cancellation never acquires the global load gate and can never enter media later. Media initialization already underway retains exclusion through cleanup. Releasing that exclusion on a timer would permit unsafe NDL overlap. A stalled network thread can still retain resources until its underlying call returns. |
| B5 | Fixed: restore the five-second scale wait, then deliver encoded covers. Later covers use a subsequently published scale. |
| H1 | Fixed: progress resets recovery counters. Underruns permit 32 consecutive prepares without an artificial sleep; would-block/interrupted stalls retain four delayed retries. Exhaustion and terminal errors still end the lane with motor fallback. The claimed hardware frequency was not measured here. |
| H2 | Applied stable source IDs allocated by the sole scanner. Source 0 remains reserved for SDL; exhaustion refuses another device rather than wrapping. The specific claimed fd race was not established: the old descriptor remains owned through its release notification. |
| H3 | Fixed: the existing Press armed/landed accessors contribute to animation scheduling. Opening resets the draw timestamp. |
| H4 | Applied a shared public-CA Agent with the existing 3-second connect and 9-second request limits. Pooling was previously excluded from finding 12's required scope; this is additional setup reuse, with no measured speedup claimed. |
| H5 | Not applied: the explicit no-tests instruction remains in force. Injected boundaries remain available for later validation. |
| M1, M8, M9 | Fixed initialization/logging and comments. The detached scanner never applies EVIOCGRAB; that remains reader-owned. Pending descriptors close on cancellation/channel teardown. |
| M3 | Restored socket flush delegation, falling back to the file on failure. |
| M4 | Deferred: transport failure still drops that attempt, matching previous behavior. Automatic retries require a separate bounded shutdown policy; this is a residual delivery limitation, not a fixed regression. |
| M5 | Fixed: actual controller removal enumerates and opens a remaining real pad, excluding the TV remote. |
| M6 | Audited: rows depend on persisted hosts/profiles, discovery, reachability and rights. Game-list arrival adds no row fields. Documented these invalidation inputs. |
| M7 | Documented the eight-result item bound. Classic decode dimensions constrain ordinary decoded results; this is not a general byte budget. Encoded console fallback remains variable-sized. |

Findings 12 and 16 remain implemented after these corrections. Finding 3 remains
partial: viewport demand, downstream retention, and a strict encoded-byte bound
are unresolved. Absence of executed tests is recorded separately from scope.

## Implementation tracking

Fifteen numbered findings have client-side implementations. Finding 3 remains
partial because the pinned shell lacks demand/eviction APIs. Candidates B/C are
implemented; A has preallocation only; D/E/F and the upstream part of G are deferred.

`cargo fmt --check` and `task docker:lint` passed. Clippy type-checks existing
test targets but does not execute tests. The only emitted compiler warnings are
the existing unstable ARM target-feature warnings (`neon`, `vfp3`, `soft-float`).
No performance gain or TV behavior is claimed as measured.

See [rollback instructions](performance-changes/README.md). The six patches
contain only implementation changes from `aa97200`, including new source files.
Every patch passed `git apply --reverse --check` against this working tree.
No commits were created. Status text must be updated if a group is reversed.

## Priority findings

### 1. P1: Cancelling reconnect can orphan a live media pipeline

**Implementation status (2026-09-14): Implemented.**

PendingConnect owns every menu/reconnect handle. Cancellation synchronizes with media entry: a cancelled network attempt cannot enter NDL and does not block future launches. Once media initialization has started, the load gate remains held through off-thread disposal. Successful abandoned connections disconnect and use existing bounded shutdown. A stalled network call can retain its worker until it returns, but cannot later acquire media ownership. Superseded failures cannot update the current launch failure flag.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [01-runtime.patch](performance-changes/01-runtime.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/runtime/stream.rs:258`, `src/runtime/mod.rs:101`,
`src/session/connect.rs:45`, `src/session/pipeline.rs:25`.

Cancelling a reconnect drops its `JoinHandle`. If the connection subsequently
succeeds, its returned `Connected` is dropped without calling `shutdown()`.
Neither `Connected` nor `MediaPipeline` stops workers on ordinary drop. The
workers retain their client and player references; the clock plane can continue
running after the menu returns. A later launch can overlap activity against
process-global NDL.

Fix: give pending connections cancellation-aware ownership. An abandoned success
must disconnect and shut down through bounded cleanup. Prevent a replacement NDL
load until that cleanup finishes, preserving the existing poison mechanism for
wedged vendor calls. Audit every early return owning a pending connection.

Validate: delay a successful handshake, cancel it, then allow completion. Check
that workers exit and cleanup finishes before another decoder load.

**Execution sequence.** The reconnect worker owns the connection attempt, while
the main thread owns only its join handle. Back during `wait_for_dial` discards
that handle and returns to the menu. Dropping a join handle detaches execution;
it does not cancel the closure. A later successful `session::connect` has already
constructed the pipeline and spawned media workers. Dropping its return value
drops their handles without setting the shared stop flag. The explicit
`Connected::shutdown` method is the missing lifetime transition.

**Impact and confidence.** The ownership gap is visible directly in code. Its
duration depends on subsequent host and worker behavior. Do not assume every
cancelled attempt leaks: attempts that fail before constructing a pipeline do
not take this path. The dangerous case is a successful abandoned attempt. The
possible effects include retained decoder resources, continued network traffic,
clock-plane work, and interference with the next load. Exact TV symptoms remain
unmeasured.

**Implementation choices.** A cancellation token should be checked before
expensive initialization and again before publishing success. Those checks alone
do not close the race between publication and cancellation. Ownership of the
published result needs an explicit cleanup path too, such as a connection owner
that transfers a live session once or disposes of it. Avoid putting potentially
seconds-long joins directly into a UI-thread destructor. A background cleanup
owner can use the existing bounded joins while excluding competing loads.

**Acceptance criteria.** Exercise cancellation before handshake completion,
during player construction, and immediately after success. Each attempt must
either transfer exactly one session or clean up exactly one session. Worker and
decoder counts must return to baseline; an intentionally wedged FFI call must
leave new loads refused rather than allow overlapping access.

### 2. P1: Persistent USB audio errors remove all loop pacing

**Implementation status (2026-09-14): Implemented.**

USB writes classify terminal errors, allow four delayed stall retries and 32 consecutive underrun prepares, and reset both counters on progress. Successful prepare retries immediately. Exhaustion or terminal failure exits the worker. USB ownership is released to restore motor fallback. An already-blocked vendor call remains outside portable cancellation.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [02-audio.patch](performance-changes/02-audio.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/platform/webos/usb_audio.rs:145`, `:217`.

The USB playback worker depends entirely on blocking `snd_pcm_writei` for pacing.
After unplug or persistent device failure, `writei` and `prepare` can fail
immediately. The loop continues draining PCM, converting samples, constructing
errors, and retrying without sleeping or exiting. Repeated logging is suppressed,
so sustained CPU consumption is largely invisible. While the worker remains
alive, it can consume a core needed by streaming.

Fix: distinguish recoverable underruns from terminal device errors. Exit and
release lane ownership on terminal failure. Bound retries and back off during
recoverable failures; keep successful writes paced by the device.

Validate: inject repeated immediate errors and unplug during playback. Assert
bounded attempts, worker termination, and correct ownership cleanup.

**Execution sequence.** Each iteration takes speaker and coil samples, fills a
four-channel output buffer, then calls `sink.write`. At 240 frames per chunk,
that is 960 sample conversions. Successful blocking playback normally supplies
the approximately 5 ms interval. The error branch only changes a logging flag.
It has no alternative timing source. `PadSink::write` also tries `prepare` for
every negative write result, rather than classifying the underlying condition.

**Impact and confidence.** Missing failure pacing is certain from the loop.
Whether a physical unplug produces prolonged spinning depends on how quickly
the owning runtime stops this worker and what ALSA returns. It is therefore a
failure-path CPU risk, not a claim that normal USB playback wastes a core. The
same loop can discard newly arriving PCM much faster than playback speed while
the sink is unusable.

**Recovery design.** Keep underrun recovery separate from device removal and
other terminal errors. A retry must have a deadline or attempt bound, and zero
progress must count toward it. Ownership currently becomes USB-owned after a
successful open; an early worker exit must also restore the intended fallback
state. Simply adding `break` without auditing that ownership could trade CPU
spinning for permanently suppressed motor output.

**Acceptance criteria.** A fake sink that always fails immediately should receive
a bounded number of calls. A sink with one recoverable underrun should resume
without terminating the entire streaming session. Measure stop latency and CPU
usage during real unplug/replug, including when no new host audio is arriving.

### 3. P2: Console art loading processes the entire library

**Implementation status (2026-09-14): Partial; upstream API required.**

The client handoff holds at most eight results, plus the producer current result. Loader drop cancels work; waiting for scale no longer accumulates an encoded parking vector. The wait is bounded to five seconds, then covers are sent encoded; later covers can adopt a late scale. Encoded fallback has variable size, so this is not a strict byte bound. Whole-library iteration and downstream shell retention remain: pf_console_ui exposes neither viewport demand nor eviction. Full demand-driven loading is NOT implemented.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/console/model.rs:975`, `:990`, `:1042`.

`spawn_art` walks every game, fetches its cover, and decodes before sending through
an unbounded channel. The consumer has an 8 ms adoption budget per service tick.
If the shelf has not published a decode scale, an unbounded `parked` vector holds
encoded covers instead. Opening a shelf therefore performs network and decode
work for unseen titles, with no explicit pending-byte bound. Moving decode off
the UI thread reduces direct stalls but still competes for the TV's CPU and RAM.

Fix: expose visible and prefetch demand to the loader. Prioritize current cards,
cancel obsolete generations, and bound decoded bytes awaiting adoption. A bounded
queue alone limits memory but does not eliminate unnecessary whole-library work.
Exact GPU retention requires inspection of the upstream shell.

Validate: open libraries of 100, 1,000, and 10,000 games without scrolling. Track
fetch/decode count, queue bytes, peak RSS, and visible-cover arrival latency.

**Why the existing safeguards are insufficient.** The adoption budget limits how
long the UI spends consuming results in one tick. It does not restrict producer
work or queued memory. The disk quota bounds encoded/raw files on disk, not
decoded messages in memory. The worker's channel-disconnection check occurs when
it sends a result; it cannot interrupt a blocking fetch. While scale is absent,
`ArtSink::push` parks data instead of sending, delaying that cancellation signal.

**Scaling.** If N covers are processed and M are visible, unnecessary work grows
with N minus M. A hypothetical 260 by 346 RGBA poster is about 351 KiB before
other allocations; 1,000 such queued images would exceed 340 MiB. This is an
illustration of byte scaling, not a measurement of the shell's actual image
format, dimensions, queue depth, or retained textures. Measure all four before
assigning a memory-saving estimate.

**Design tradeoffs.** A request-driven bridge likely needs an upstream shelf API
for visible IDs or index ranges. Publish demand only when it changes. Keep a
larger retention band than prefetch band to avoid oscillation. Bound bytes as
well as item count because covers can differ in size. If a bounded producer
waits for capacity, dropping the receiver must unblock it; switching shelves
must never join a producer whose queue nobody drains.

**Acceptance criteria.** An untouched shelf should fetch approximately its
prefetch demand, regardless of catalog size. A direct scroll to the end should
prioritize the new viewport over abandoned work. Warm-cache tests matter too:
fast disk reads and decodes can expose queue growth that slow networking hides.

### 4. P2: Late classic cover results escape eviction

**Implementation status (2026-09-14): Implemented.**

Per-request cancellation tokens invalidate evicted jobs and late results, including stale completions after a re-request. Adoption relies on loader token validity, not arrival membership, which regrouping clears while requests remain live. Results are bounded to eight queued entries; ongoing HTTP calls finish under their request budgets.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/app/mod.rs:511`, `src/app/render/prepare_grid.rs:109`,
`src/services/art.rs:658`, `:694`.

Classic art requests remain queued after a card leaves the retained window.
`forget()` only changes the request bookkeeping; it does not cancel worker work.
When an evicted card finishes later, `drain_art` inserts its pixels unconditionally.
Eviction examines `grid.arrivals`, from which that ID was already removed. The
late allocation therefore cannot be found by later eviction passes unless that
card re-enters the build window. Rapid scrolling can accumulate offscreen pixels
and repeated obsolete requests.

Fix: attach generation/window validity to requests and results. Reject obsolete
results before adopting pixels, and cancel queued work outside retained demand.
Ensure every resident image participates in eviction independently of animations.

Validate: delay cover completion until after eviction, then deliver it. Assert
decoded residency and pending work stay bounded across repeated long scrolls.

**Concrete interleaving.** First, `build_card_window` requests cover A and records
A in `arrivals`, even though its pixels have not arrived. Next, scrolling moves A
outside the keep window; eviction removes its arrival entry and calls `forget`.
The worker then finishes A. `drain_art` inserts it into `library.art` and marks it
dirty, but does not restore an arrival entry. Later eviction enumerates only
arrival IDs, so it never visits A. Removing an entry from `render.covers` while
processing dirty IDs does not remove the decoded pixels from `library.art`.

**Scope.** This is retained memory within the library's lifetime, not necessarily
a permanent process-lifetime leak. A library reset or revisit may reclaim it.
It requires delayed completion relative to scrolling; browsing slowly enough
for all results to arrive before eviction can conceal it. Re-requesting a
forgotten ID can additionally leave multiple jobs for the same image outstanding.

**Fix boundaries.** A host/catalog generation rejects results belonging to an old
catalog, but does not by itself reject results for an old viewport in the same
catalog. Check current retained demand too. Conversely, invalidating every job
on every pixel of scrolling would waste useful work. Keep accepted overlap and
cancel only jobs leaving the retention band. Any replacement arrival bookkeeping
must still allow animation state to expire independently of image ownership.

**Acceptance criteria.** Drive the sequence above deterministically with a held
worker result. Verify A is neither adopted nor retained after release. Repeat
with scroll-away/scroll-back before completion; the still-wanted result should
remain usable without an unnecessary second decode.

### 5. P2: Collection headings invalidate cover windows

**Implementation status (2026-09-14): Implemented.**

Page/build/keep windows now derive from group-aware visible_cards geometry, with separate row padding for prefetch and retention.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


Source: `src/app/render/prepare_grid.rs:52`.

The preparation path derives rows from `scroll / row_h`, deliberately ignoring
heading offsets. Rendering includes cumulative group heading spacing. Prefetch
slack cannot absorb an offset that grows with the number of collections. At the
bottom of a sufficiently grouped library, the build window can be empty while
cards remain visibly on screen. Those covers remain unrequested if browsing
skipped their earlier, incorrectly calculated request window.

Arithmetic example using a 1920 by 1080 layout: five columns, 260 by 346 cards,
370 row stride, and 21 groups of five cards. Heading offsets total 1900 pixels;
maximum scroll is 8782. Preparation computes first row 23 and build start
`(23 - 2) * 5 = 105`, the catalog end. The final cards still draw at y=678.
This is a source-derived geometry example, not an executed UI test; actual layout
dimensions also depend on scaling.

Fix: derive page/build/keep ranges from the same group-aware visible geometry used
by painting and hit-testing, then extend by prefetch/retention rows.

Validate: direct bottom scrolling with the maximum collection count, partial
rows, multiple widths, and UI scales. Every visible card must belong to build and
keep demand.

**Underlying mismatch.** A group's screen position contains both row distance
and accumulated heading distance. Dividing scroll by row stride treats headings
as if they were card rows. The resulting index moves farther ahead of the real
viewport after every group. Fixed prefetch slack hides the error near the start,
but cannot compensate once total heading distance exceeds that slack.

**Other affected behavior.** The same calculation supplies `page_window` and
`keep_window`, not just download requests. Page readiness can ask about the wrong
cards, including an empty range that appears ready. Retention can discard art
that rendering still needs. Fixing download priority alone leaves these two
inconsistencies in place.

**Implementation caution.** Use the existing group-aware visibility calculation
as the authority. Depending on its return type, demand may be several bounded
bands rather than one naive flat interval. Include partial-row padding correctly,
and preserve the separate focus-window invariant: a focused card must remain a
valid navigation origin even during scrolling or animation.

**Acceptance criteria.** Assert the stronger invariant that every painted card
is inside both build and retention demand. Cover empty collections, partial last
rows, direct jumps, and reordered collections. An arithmetic unit test can prove
the invariant without comparing screenshots or depending on a TV.

### 6. P2: Eviction repeatedly scans the complete catalog

**Implementation status (2026-09-14): Implemented.**

Eviction constructs retained IDs from the keep window and checks resident membership; it no longer reverse-scans the entire game catalog for each resident.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/app/render/prepare_grid.rs:110`, `src/app/grid.rs:142`.

Each resident ID is mapped back to a slot using `games.iter().position(...)`.
Each retention-window change therefore costs O(resident cards times library
size). Hysteresis reduces frequency, not complexity. This violates the stated
O(visible) requirement on scrolling.

Fix: maintain an ID-to-position index when the catalog changes, or derive retained
IDs from the bounded keep window. Account for regrouping and reorder operations.

Validate: measure identical scroll sequences across increasing library sizes.
Per-window eviction work should depend on retained cards, not total titles.

**Cost model.** Let R be resident IDs and N catalog length. Finding an ID near
the end costs approximately N string comparisons; doing so for R IDs approaches
R times N. For illustration, 60 resident IDs and 10,000 catalog entries allow
roughly 600,000 comparisons on one eviction pass, before bounded group lookup.
This is an operation-count example, not a measured frame duration.

**Preferred small change.** Construct the wanted ID set by walking the bounded
keep window, then compare resident IDs against that set. This directly expresses
eviction and avoids a new catalog-wide index if no other caller needs one. A
persistent index is worthwhile if several navigation paths also need reverse
lookup, but every reorder and catalog replacement must invalidate it correctly.

**Acceptance criteria.** Instrument lookup/comparison counts separately from wall
time. Increasing N while holding visible and retained card counts constant
should not increase eviction work. Include grouped layouts and padding slots.
Avoid optimizing this by scanning `library.art` if finding 4 still allows that
map to grow with the entire browsing history.

### 7. P2: An unchanged disconnect dialog redraws continuously

**Implementation status (2026-09-14): Implemented.**

Dialog lifecycle advances independently of drawing. Changes redraw immediately; animation draws are spaced by 16 ms. The press dip participates in animation scheduling, and reopening clears the previous draw timestamp. A stationary dialog stops submitting frames, and dismissal still wipes or completes the pending outcome.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [01-runtime.patch](performance-changes/01-runtime.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/runtime/stream.rs:853`, `src/runtime/overlay.rs:269`.

`disconnect.frame()` returns a frame throughout the dialog's open lifetime.
The stream loop renders it on every iteration, even after animation settles.
This bypasses the stats/log overlays' 33/500 ms cadence. The loop sleeps only
2 ms; actual rendering frequency depends on raster and swap blocking. Unchanged
content keeps spending CPU/GPU work while the video stream continues.

Fix: render on state changes and scheduled animation ticks. Advance dialog
lifecycle independently of whether a frame is submitted, and retain the final
transparent wipe when dismissal finishes.

Validate: count submissions during ten seconds of an untouched dialog. After
animations settle, no repeated submissions should be necessary. Check opening,
pointer navigation, confirmation, cancellation, and fade completion.

**Why cadence matters.** An opaque video plane continues presenting beneath the
transparent GL surface. A static dialog does not change those pixels, so a new
Skia draw and swap adds no visible information. If swap waits for vsync, it can
also occupy the main thread that handles input and feedback. If swap does not
block, rendering may run more frequently. Neither behavior implies a specific
FPS or a guaranteed audible/video glitch without measurement.

**Scheduling contract.** Separate three states: visible, animating, and dirty.
Visibility keeps the existing surface on screen; animation provides a next
deadline; dirty state requests a new submission. Events such as hover movement,
button focus, press state, display changes, and dismissal must invalidate the
dialog. Continue lifecycle ticking while no frame is drawn, including the final
close transition that releases a pending disconnect outcome.

**Acceptance criteria.** Count draws, swaps, and main-loop latency independently.
After settling, the dialog should contribute no repeated draws until a relevant
event. A close animation must terminate and clear exactly once. Compare stream
pacing counters with the dialog closed, animating, and stationary.

### 8. P2: Feedback coalescing drops the newest state

**Implementation status (2026-09-14): Implemented.**

A replaceable latest-state mailbox replaces depth-one try_send queues on USB and Bluetooth. Final release closes the mailbox against later updates. Transport work runs outside its mutex.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [02-audio.patch](performance-changes/02-audio.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/platform/webos/dualsense.rs:285`, `:304`, `:354`.

The depth-one channel uses `try_send`. A full queue preserves its older value and
drops the new absolute state. That final state remains only in `Feedback.state`,
with no guaranteed subsequent send. A burst ending in trigger release can leave
the earlier resistance active. The 250 ms subprocess throttle widens this window.

Fix: use a replaceable latest-state slot with a wakeup. Preserve the documented
transport throttle; removing it would reintroduce measured compositor starvation.

Validate: block the sender, issue several distinct states ending in release, then
unblock it. The newest state must be delivered without another host event.

**Minimal sequence.** The sender is busy transmitting A. Update B occupies the
one queued slot. Update C arrives before B is consumed. `try_send(C)` returns
`Full(C)` and the ignored error drops C; B remains queued. If C disables trigger
resistance and no subsequent update arrives, the hardware receives B and never
receives C. A later update often hides the bug because it carries the full state.

**Scope distinction.** This finding concerns ordinary host updates through
`apply`, including a host-sent effect that clears a trigger. The explicit
end-of-session `release()` uses blocking `send`, so its separate loss mechanism
is finding 9. Combining the two in a test would make the root cause ambiguous.

**Mailbox semantics.** These messages are complete desired states, which makes
replacement appropriate. The producer should update a latest-state slot and
notify the sender without waiting for transport I/O. The sender snapshots the
state and performs FFI or subprocess work after releasing any mailbox lock.
Define shutdown release as a final state that later ordinary updates cannot
overwrite. Do not apply this dropping policy to unrelated ordered input edges.

**Acceptance criteria.** Assert the final transmitted state, rather than expecting
every intermediate state. Both USB feedback and Bluetooth fallback construction
use the depth-one channel and deserve coverage, even though their throughput
and likelihood of filling it differ.

### 9. P2: LS2 feedback can strand pending updates or final release

**Implementation status (2026-09-14): Implemented.**

Pending state supplies a send deadline even without an audio envelope. Closure drains final release through the throttle before sender exit. Failed sends remain bounded attempts, not promises of hardware delivery.

**Residual delivery limitation (M4):** failed transport attempts are not automatically retried. Final release is drained and attempted, but successful physical delivery is not guaranteed.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [02-audio.patch](performance-changes/02-audio.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/platform/webos/dualsense.rs:374`, `:783`.

With in-process LS2 and no coil envelope, an update arriving inside the 16 ms throttle
window becomes `pending`. The next iteration calls unbounded `recv()`, so silence
from the host leaves that pending state unsent. The lane is constructed only when
both the LS2 bus and a coil envelope exist; quiet audio is not equivalent to an
absent envelope, because an existing lane continues providing timer wakeups.

| Transport configuration | Can strand pending until another event? | Can discard a not-yet-due release on closure? |
| --- | --- | --- |
| USB hidraw | No. Separate unthrottled sender loop; no pending slot. | No, not through this mechanism. Queued states drain before receive reports closure. |
| Bluetooth subprocess | No. The full interval is slept after sending, before the next receive. | No, not through this mechanism. The next state is already due when received. |
| Bluetooth LS2, no coil envelope | Yes. Throttled pending state has no timer. | Yes. Closure exits before pending flush. |
| Bluetooth LS2, coil envelope present | No indefinite idle stall; audio ticks wake the sender. | Yes. Closure can arrive before the next permitted send. |

This matrix concerns successful transport operation and the pending/deadline bug.
Transport failures can still prevent delivery on any route. Finding 8's
drop-newest queue behavior is also separate.

Fix: on LS2, wait until the earliest pending-send deadline or audio tick. Flush
the final release on channel closure with bounded failure handling. USB's sender
and the subprocess throttle need no deadline redesign for this finding.

Validate: on LS2 without an envelope, send two states less than 16 ms apart
followed by silence. On LS2 with and without an envelope, release and immediately
drop the sender. USB/subprocess cases are regression controls, not additional
affected configurations.

**Deadline example.** Suppose A is sent at t=0 and B arrives at t=5 ms on a
16 ms route. B becomes pending because it is not due. Without an audio lane, the
next operation waits for another message rather than t=16 ms. If the host stops
updating, B stays pending indefinitely. The sender needs a timer even when no
audio work exists.

**Shutdown example.** A final release can be received inside that same throttle
window. It becomes pending, then `Feedback::drop` closes the channel. The next
receive reports disconnection and exits the loop before pending is sent. Joining
only proves the thread exited; it does not prove the hardware received release.
An existing audio lane prevents indefinite idle waiting, but does not eliminate
this close race. The reviewer's statement that shutdown is configuration-independent
is too broad: only LS2 can reach the not-due pending state described here.

**Implementation caution.** A wakeup should be scheduled for the minimum of audio
deadline and pending-state deadline. When disconnected, transition into a
bounded final-flush state instead of immediately returning. Preserve the
subprocess route's longer interval. Define what happens if the device has gone
away, so guaranteed attempt does not become an unbounded return-to-menu wait.

**Acceptance criteria.** Use a controllable clock and fake transport for the two
sequences. Check that B is sent at the next permitted time without a third event,
and that teardown attempts the final release before joining completes. Test
transport failure separately from intentional rate limiting. Keep these tests
focused on LS2 scheduling rather than auditing every transport anew.

### 10. P2: Input gating loses key and mouse-button releases

**Implementation status (2026-09-14): Implemented for keyboard/mouse edges.**

Forwarded key/button state is tracked by source across evdev and SDL fallback. Deactivation, unplug and stop synthesize releases without releasing another source held key. Each report reads the live gate after poll. A generation counter preserves deactivation releases even across a rapid close/reopen. Scanner-assigned source IDs are never recycled within the reader; 0 is reserved for SDL. This does not redesign gamepad button-state handling.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [01-runtime.patch](performance-changes/01-runtime.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/platform/webos/evdev/mod.rs:229`, `:251`, `:699`.

Opening the disconnect dialog gates ordinary HID reports. Keyboard and mouse
buttons use edges, not complete held-state snapshots. Hold W or a mouse button,
open the dialog, release while it is open, then cancel: the host never receives
the release. Device removal similarly lacks a forwarded key/button release
ledger. Touch-contact cleanup does not cover these events.

Fix: track forwarded presses per device. Synthesize releases before gating,
unplugging, or stopping; serialize this with forwarding. Audit SDL events swallowed
by the dialog under the same invariant.

Validate: each forwarded press receives exactly one release across those
transitions, with no stuck input after cancellation.

**Edge versus state.** Raw keyboard and mouse-button reports encode changes.
Receiving another motion event or another key does not implicitly release W.
The reader's comment that keys and clicks self-clear on the next report is
therefore not true for these decoded events. Touch reports have their own cleanup
path, which cannot clear a keyboard press.

**Concurrency boundary.** Merely checking held keys on the UI thread before
changing an atomic flag can race the reader forwarding another press. Keep
forwarded-state accounting and the release transition on the reader's owner
thread, with an explicit deactivate request or equivalent serialization. Track
what was actually sent, not just what is physically down, to avoid releasing
keys that were pressed only while the dialog owned input.

**Multiple devices.** If two keyboards hold the same key, releasing one device
must respect the protocol's representation of the remaining hold. Decide whether
the forwarding layer merges those holds or identifies devices. On reactivation,
also define how a still-held physical key resumes; a fresh press policy and a
state-resynchronization policy have different user behavior.

**Acceptance criteria.** Test keyboard and mouse buttons separately. Include
release during dialog, unplug during hold, session stop, and two devices holding
the same key. Record outbound events rather than relying solely on in-game feel.

### 11. P2: Hotplug scanning blocks existing raw input

**Implementation status (2026-09-14): Implemented.**

A separate scanner probes devices, checks stop between nodes, and hands descriptors through a bounded channel. Reader owns grabs, forwarding and unplug cleanup. Initial empty presence and its startup log are restored. The intentionally detached scanner never grabs a node. Scanner wakeups are bounded; a currently blocked open can outlive cancellation without owning a grab.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [01-runtime.patch](performance-changes/01-runtime.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/platform/webos/evdev/mod.rs:342`, `:607`.

The sole input reader synchronously probes nodes when `/dev/input` changes. The
documented device measurements are approximately 40 ms per open and approximately
20 empty nodes. The mtime gate prevents repeated idle scans, but a real hotplug
still makes existing devices wait behind slow probes. Cancellation and thread
joining can also wait through that scan, beyond the advertised poll timeout.

Fix: probe on a separate worker and transfer opened descriptors to the reader.
Check cancellation between probes and preserve reader ownership of forwarding
and grab transitions.

Validate: inject slow node opens while delivering mouse reports. Measure input
delivery gaps and shutdown latency during scanning.

**Execution sequence.** Existing file descriptors are polled and drained, then
the reader checks the rescan interval and directory mtime. When changed, `scan`
runs synchronously before the next poll. Previously unopenable nodes are retried
because some failures are transient. During their opens, no existing descriptor
is serviced by this thread. Kernel buffering may preserve events, but delivery
latency still increases and finite queues can overflow under sufficient delay.

**Magnitude and confidence.** Multiplying the notes' approximate 20 nodes by
40 ms suggests an approximately 800 ms worst-case scan contribution on the
documented setup. This is extrapolation from prior measurements, not a fresh
measurement of every hotplug. It is intermittent, because the mtime gate already
eliminates unconditional scans.

**Design alternatives.** A discovery worker can open/probe candidates while the
reader continues input handling. Transfer descriptors only after validating
they still represent the intended device. Alternatively, spreading probes across
reader iterations limits cumulative interruption but still leaves each slow
open on the input path. A cancellation check between nodes bounds further work,
not an already blocked open. Avoid spawning one worker per node or holding a
device-list mutex while probing.

**Acceptance criteria.** Test churn: attach, detach, and reattach while a scan is
running. Ensure no duplicate descriptors or leaked grabs, and that stale probe
results are ignored after shutdown. Measure maximum gaps, not just average input
throughput.

### 12. P2: External cover downloads have no configured deadline

**Implementation status (2026-09-14): Implemented.**

External art requests now use the existing REQUEST whole-request budget (9 seconds) and PROBE connect cap (3 seconds), preserving default public-CA verification. Each fallback URL retains its own budget. One process-wide public-CA Agent reuses transport setup and connections without the host identity or pin.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [04-art-deadlines.patch](performance-changes/04-art-deadlines.patch).
Apply only the whole group; shared files couple findings within it.


Source: `src/services/library.rs:244`.

External URLs use a fresh `ureq::Agent::new_with_defaults()` on each request.
Unlike the pinned host agent, this path sets no request or connect budget. The
current working lockfile pins ureq 3.4.2. Its `Timeouts::default` sets global,
per-call, resolve, connect, send-request, send-body, receive-response, and
receive-body deadlines to `None`. Only `await_100` defaults to one second; that
does not bound an art GET. A stalled CDN can therefore occupy the single art worker indefinitely,
blocking every subsequent cover and queued hero. Receiver removal cannot cancel
a request already blocked inside the transport. The original 3.4.0 reference
matched HEAD at review time; this statement now records the verified updated
dependency rather than implying the working lockfile still contains that version.

Fix: configure explicit connect and whole-request deadlines for the external
HTTPS path. Preserve public CA verification and keep client certificates confined
to the pinned host transport. Combine this with stale-request cancellation.
Agent lifetime/reuse is optional and is not needed to fix the deadline bug.

Validate: serve headers or partial bodies and stall. Subsequent visible art must
advance within the configured budget.

**Blocking path.** `fetch_art` recognizes an absolute URL and delegates to
`fetch_external_art`, bypassing the host agent's configured budgets. A server can
accept TCP and then stop during TLS, headers, or body transfer. With no global
deadline, channel cancellation and hero prioritization cannot run until that
call returns. This affects both callers of the shared fetch helper; a classic
hero cannot jump ahead of an already executing stalled cover request.

**Timeout policy.** Choose separate short connect and finite whole-request
budgets. A read-idle timeout alone can be defeated by a server delivering tiny
pieces forever. Account for trying multiple fallback URLs: three individually
bounded attempts can still multiply visible waiting time. A request generation
check between attempts prevents finishing every fallback for obsolete demand.

**Trust boundary.** Do not reuse the host's permissive/pinned verifier or client
identity for a CDN. No pooling speedup is claimed here: origin distribution,
server keep-alive, and response handling determine whether reuse would help.
One-shot URLs can share an origin, so distinct URLs alone prove neither benefit
nor lack of benefit.

**Acceptance criteria.** Stall at each network phase, then verify the queue
advances. Add a slow trickle response to test the whole-request limit. Verify
fallback URLs still work, certificate verification remains enabled, and host
credentials are absent from external connections.

### 13. P3: Opt-in telemetry can delay diagnostic launches

**Implementation status (2026-09-14): Implemented.**

Opt-in telemetry has a 500 ms overall startup wait including background DNS, plus 500 ms TCP write timeout. Write failure permanently switches that sink to an already-open rotating file. A blocked DNS resolver may outlive startup on its detached worker; no reconnect loop is added.

Socket flush delegates to the live stream and switches to the fallback file on failure.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [05-telemetry.patch](performance-changes/05-telemetry.patch).
Apply only the whole group; shared files couple findings within it.


Sources: `src/logger/sink.rs:59`, `src/runtime/mod.rs:239`.

Logger initialization synchronously calls `TcpStream::connect` when telemetry is
configured. A blackholed destination waits on OS connection behavior before the
file fallback can run. The asynchronous appender only helps after this initial
connection; it does not protect startup. The connected stream also has no write
timeout, allowing a stalled receiver to park the logging worker.

**Reachability.** `logger::launch::telemetry_addr` returns `None` unless argv's
launch JSON contains a nonempty telemetry address. Ordinary installed-app
launches therefore use file logging and cannot hit this network startup wait.
`task deploy TELEMETRY=...`, the container preview, or an explicitly parameterized
launch can hit it. This is primarily a developer/diagnostic workflow hazard,
not an ordinary shipped startup regression. It is not compile-time dead code
in installed builds; the same binary can receive those launch parameters.

Fix: impose a short startup connection budget, including hostname resolution
where accepted. Bound writes and define file fallback after transport failure.

Validate: launch against a silently dropping endpoint and a connected endpoint
that stops reading. Check startup latency and continued useful logging.

**Startup versus steady state.** The startup issue occurs before the asynchronous
appender exists: `init_subscriber` must obtain its sink first. A refused localhost
port may fail immediately and conceal the problem; a silently filtered address
is the relevant test. Once connected, a blocked TCP write occurs on the appender
worker rather than directly on the video pump. It can cause log backlog or loss;
it should not be described as directly blocking every producer.

**Implementation choices.** For a numeric socket address, a bounded connect is
straightforward. Hostname input also needs bounded resolution or an asynchronous
startup path. An alternative is to start with a file sink and establish telemetry
in the background, defining whether buffered records are forwarded. Either way,
keep destination failures out of the UI's critical startup path.

**Failure policy.** A write timeout without a recovery policy merely converts a
hang into discarded output. Decide when to switch to file logging and whether
reconnection is attempted. Recovery itself needs backoff and bounded memory.
Avoid recursively logging sink failures through the same failing sink.

**Acceptance criteria.** Measure time to first usable UI with telemetry disabled,
reachable, refused, filtered, and stalled after connect. Confirm records remain
available locally after fallback and that quitting is not delayed indefinitely.

### 14. P2: Partial USB PCM writes discard remaining samples

**Implementation status (2026-09-14): Implemented.**

USB PCM writes advance by accepted interleaved frames, preserving partial tails. Would-block/interrupted stalls allow four 5 ms waits. Underruns have a separate allowance of 32 consecutive prepares without a post-prepare delay. Positive writes reset both counters. Terminal errors stop the lane.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [02-audio.patch](performance-changes/02-audio.patch).
Apply only the whole group; shared files couple findings within it.


Source: `src/platform/webos/usb_audio.rs:145`.

Every nonnegative `snd_pcm_writei` result counts as complete, even when fewer
frames were accepted. The next iteration consumes fresh PCM, discarding the
unwritten tail. The frequency of partial writes on this hardware is unmeasured.

Fix: advance through accepted frames. Handle zero progress, recoverable underruns,
and terminal errors with bounded behavior, coordinated with finding 2.

Validate: inject partial frame counts and zero progress; verify correct sample
order without an infinite retry loop.

**Example.** A 240-frame chunk is submitted and the device reports accepting
120 frames. The current function returns success. The next loop removes a fresh
240-frame chunk from the envelope, so the previous final 120 frames disappear.
For four interleaved channels, retrying must advance by 120 times four samples,
not 120 samples. A zero return currently drops the entire submitted chunk.

**Recovery tradeoff.** Retrying a partial write preserves its tail. Recovering
after an underrun is a separate decision: preserving all stale audio can add
latency, while deliberately dropping it can create a discontinuity. Make that
policy explicit rather than treating all nonnegative results as success and all
errors identically. Terminal failures should use finding 2's bounded exit path.

**Acceptance criteria.** Feed a sequence with distinguishable frame values into
a fake sink accepting 120, then 60, then 60 frames. Its accepted output must equal
the original 240 frames exactly once and in order. Add repeated zero progress,
one recoverable error, and unplug after a partial write. Actual partial-write
frequency on the TV remains an open measurement question.

### 15. P2: Unrelated controller removal clears the real pad handle

**Implementation status (2026-09-14): Implemented.**

Classic controller-removal handling checks instance identity. Unrelated removals keep the owned controller and navigation state. Actual removal enumerates and opens a remaining real controller, excluding the TV remote.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [01-runtime.patch](performance-changes/01-runtime.patch).
Apply only the whole group; shared files couple findings within it.


Source: `src/runtime/ui_flow.rs:284`.

The classic menu sets `controller = None` for every removal event. Console flow
already filters by instance ID. An unrelated Magic Remote removal can therefore
drop the real pad handle and clear its navigation state. The existing console
comments document frequent remote disconnects.

Fix: compare the removed instance with the owned controller. If the actual pad
was removed, select an appropriate remaining device.

Validate: remove an unrelated device while a gamepad remains connected; its
handle and navigation should remain functional.

**Event identity.** `ControllerDeviceRemoved` identifies an instance, not a request
to clear whichever controller the application happens to own. The classic branch
ignores that identity. It recalculates whether any pad remains, but still drops
the owned handle and clears chord/repeat state. Consequently the presence flag
can say a pad exists while the handle used for pad-specific work is absent.

**Scope and confidence.** Incorrect handle clearing is directly visible. The
exact amount of navigation lost depends on what SDL continues delivering after
handle closure and which parts of the loop require that handle. Treat complete
pad loss as a possible symptom, not a guaranteed outcome of every unrelated
removal event. Console flow provides an existing instance-filtering precedent.

**Acceptance criteria.** Simulate removal of instance B while instance A is owned;
A's handle and armed navigation state should survive. Remove A and ensure its
held state is cleared, then select another eligible pad if present. Do not
confuse device-index IDs from add events with instance IDs from remove events.

### 16. P3: Failed settings writes suppress identical retries

**Implementation status (2026-09-14): Implemented.**

Writes report errors and retry the latest document at most three times with 250/500 ms backoff. New snapshots supersede retries; an identical save after exhausted retries is accepted. ConsoleStore always forwards saves to the writer, even if its in-memory settings already match; no-op saves do not bump its row revision. Shutdown skips retry delays and stops retrying after a failed final attempt. Filesystem calls themselves remain blocking on the writer.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback groups:** [06-settings-writer.patch](performance-changes/06-settings-writer.patch) for writer recovery; [03-art-and-models.patch](performance-changes/03-art-and-models.patch) for ConsoleStore forwarding.
Apply only the whole group; shared files couple findings within it.


Source: `src/services/store/writer.rs:64`, `:84`.

The worker discards `save()` errors. Deduplication compares against the last
queued state, which was updated before persistence succeeded. A transient write
failure followed by another save of the same state is therefore suppressed. The
user receives neither a warning nor a retry; restart can lose the change if no
later successful document save persists it. This is a persistence failure-path
improvement, not an established UI performance bottleneck.

Fix: distinguish accepted/pending state from successfully persisted state. Report
failure and retry with bounded backoff, while allowing newer snapshots to replace
older ones. Avoid introducing an immediate retry loop on full storage.

Validate: fail one write, restore storage, and save the same state. The latest
document must eventually persist without reverting newer changes.

**State sequence.** Disk contains A. The UI requests B, so `queue.last` becomes B
and pending B is handed to the worker. The disk operation fails. Pending is now
empty, but `queue.last` remains B. A later `save(B)` compares equal and returns
without attempting persistence. Restart reloads A if no later successful save
occurs. A later successful C carries the whole current document, including B's
field if it is still desired, and repairs the earlier persistence failure.
An identical re-save is useful to demonstrate deduplication suppressing retry;
it is not a prerequisite for loss. One failed final save followed by restart is
enough when no other successful write occurs.

**Concurrency design.** Preserve one serialized disk writer. The queue's latest
desired state and the last successfully persisted state have different meanings.
On failure, retain enough information to retry, but never let an old retry replace
a newer desired snapshot. A generation counter can make completion attribution
explicit. Perform serialization, writes, delays, and error reporting outside
the queue mutex wherever possible.

**Shutdown policy.** The existing drop waits for the writer. Adding retries must
not make permanent storage failure turn that join into an infinite wait. Define
a bounded shutdown attempt and report that persistence failed. Preserve the whole
shared document and atomic replacement; optimizing by reconstructing only known
settings fields would introduce data loss in other clients' settings.

**Acceptance criteria.** Test A-to-B failure followed by identical B, B failure
followed by newer C, and permanent failure during shutdown. Persisted state must
never regress from C to B, UI saves must remain nonblocking on disk I/O, and
failure must be observable.

## Additional optimization candidates

These are not additional established user-visible problems. Prioritize art and
rendering work above. Treat callback preallocation as opportunistic hardening;
defer management reuse, pad-lock cleanup, and Bluetooth allocation work unless
profiles or observed stalls justify them. Static patterns alone cannot rank
their real cost.

- **SDL callback allocation and diagnostics.** `src/platform/webos/audio.rs:103`,
  `:235`, `:295`: the ring starts without capacity and grows in the callback.
  Failed recycling can free vectors there. Debug logging formats events on the
  callback thread and can acquire the overlay log-ring mutex. Preallocate bounded
  storage before device resume and publish counters for another thread to log.
  Measure callback duration tails and allocation counts. Preserve JitterPolicy
  and the proven 512-frame device request. This is avoidable deadline variability,
  not evidence that it currently causes audible glitches.

- **Unchanged console models rebuild up to ten times per second periodically.**
  `src/console/model.rs:164`, `:181`: `rows()` clones the whole persisted document,
  clones profiles, builds host and pin rows, and sorts them before `set_hosts`
  decides nothing changed. Use store/discovery/reachability generations and
  targeted reads. This also shortens mutex hold time in `ConsoleStore::snapshot`.
  `Service::tick` returns early within its own 100 ms gate, even though called
  each frame. Commands can additionally rebuild rows; sixty periodic rebuilds
  per second is not supported by the current source.

- **Console cache pruning walks the directory per cover.**
  `src/services/art.rs:328`, `:446`: each `store_cover` scans metadata and sorts
  cache files, even below budget. The classic worker already carries totals to
  avoid that pattern. Share budget accounting or prune in batches, preserving
  bounds and handling concurrent workers. Measure filesystem calls on a cold
  shelf load before and after.

- **Repeated management polls rebuild transport state.**
  `src/services/status.rs:39`, `src/services/library.rs:134`: each status request
  creates a new agent, parses PEM credentials, and loses connection reuse.
  A per-host management worker/agent could share polls, library reads, and rights
  checks. Invalidate it on identity, pin, or address changes. This runs off-thread
  and at low cadence, so prioritize the larger art and rendering issues first.

- **Pad PCM locks include per-sample work.**
  `src/session/pad_audio.rs:220`, `:270`, `:389`: consumers pop samples individually
  while locked, and decimation also runs within its lock. Bulk two-slice copies
  and computing small blocks before acquisition would shorten hold times. These
  are bounded locks; contention has not been measured and no deadlock was found.

- **Bluetooth report construction allocates repeatedly.**
  `src/platform/webos/dualsense.rs:572`, `:876`,
  `src/platform/webos/ls2.rs:144`, `:226`: vectors, JSON strings, C strings, and
  owned replies are created around the approximately 94 reports/second audio
  lane. Reuse serialization buffers where ownership permits. Preserve CRC,
  sniff transitions, prefill, and report cadence.

- **Further rendering work needs upstream visibility.**
  `src/runtime/console_flow.rs:473` redraws every loop iteration; after 60 seconds
  idle, an unconditional 16 ms pre-draw sleep reduces cadence but does not stop
  redraws. This differs from the active loop's remaining-budget sleep. Upstream
  animation/dirty deadlines could support event-driven draws.
  Classic modal animation also repaints its unchanged home backdrop
  (`src/runtime/ui_flow.rs:415`). Consider caching that base scene only after
  measuring CPU and GPU costs separately. The CPU cover-blur cache
  (`src/app/draw/glass.rs:145`) changes keys with rounded sigma during zoom;
  measure cache misses before altering materials.

### Candidate A: Make callback work predictable

**Implementation status (2026-09-14): Partial; accepted small change implemented.**

Preallocated callback ring for policy depth plus 64 ordinary 5 ms queued chunks before resume. Longer combined concealment bursts can still grow capacity. Callback diagnostics and recycling behavior remain unchanged and deferred.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [02-audio.patch](performance-changes/02-audio.patch).
Apply only the whole group; shared files couple findings within it.


The software callback drains incoming PCM vectors into its `VecDeque`, then asks
the jitter policy how much to serve or discard. Starting with zero capacity
guarantees initial growth in the callback. Further growth depends on the maximum
burst admitted before policy trimming. Reserving just the nominal target depth
is insufficient if a callback first drains a larger queued burst.

Choose capacity from a documented upper bound on incoming samples, callback
quantum, and retained policy depth. Consider capping adoption before extending
the ring, but preserve the policy's crossfaded trimming semantics. An arbitrary
early drop could reintroduce clicks. Preallocating also does not make standard
channels lock-free; avoid claiming the callback is entirely free of synchronization.

For diagnostics, publish cumulative counters and let an existing non-callback
thread format periodic messages. A consistent snapshot need not mean adding a
mutex that the callback must acquire. Atomics or explicitly approximate counters
may suffice, depending on how the values are used.

Measure allocations during startup, re-priming, and a full input queue, with
debug logging both enabled and disabled. Track worst callback duration alongside
underruns. The expected benefit is reduced tail variability, not a lower configured
audio latency floor. Changing buffer quantum or jitter tuning is outside this fix.

**Disposition after review.** Preallocation need not wait for a full TV profiling
campaign: it is a small, reversible hardening change once a safe capacity is
justified. Ordinary steady-state growth should subside after capacity is reached,
but "within the first second" is unmeasured and later bursts can require more.
The diagnostic line occurs only once per 1,000 callbacks and at debug level.
There is no evidence here of ongoing audible glitches or a large performance win.

### Candidate B: Rebuild console host rows only when inputs change

**Implementation status (2026-09-14): Implemented.**

ConsoleStore exposes a change revision. Periodic row reconstruction now occurs only on store revision or command/discovery/reachability invalidation, not every unchanged 100 ms service tick. Audited row inputs are persisted hosts/profiles, discovery adverts, reachability and rights; game arrivals add no fields.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


The service's 100 ms cadence serves background work, but it also calls `rows()`
unconditionally. `rows()` snapshots the persisted document under its mutex,
duplicates profile data, constructs host and pinned-profile rows, and sorts the
result. The downstream equality check happens after this work. A large per-game
settings document increases snapshot cost even if only one host row is displayed.

The cadence is enforced at `src/console/model.rs:144` using `SERVICE_EVERY` from
line 42. It is independent of both the frame loop's 16 ms budget and the ten-second
discovery sweep. The gate timestamps actual service work, so periodic rebuilds
may be less frequent than ten per second depending on frames. Event-triggered
rebuilds remain possible outside that periodic path. The reviewer's sixfold
cost increase therefore does not apply.

Introduce a model-dirty generation driven by actual dependencies: persisted host
or profile edits, discovery answers, reachability changes, and any status fields
used by the rows. Incrementing a generation for every service tick would defeat
the optimization. A targeted read can avoid cloning unrelated settings, but
building the entire model while holding the store lock would merely move the
cost into a longer critical section.

Validate that every visible field still refreshes after its source changes.
Measure allocation count and CPU time while idle with small and large settings
documents. Expected acceptance: no host-row reconstruction on unchanged ticks,
without delaying command handling or background job adoption.

### Candidate C: Batch cache accounting and pruning

**Implementation status (2026-09-14): Implemented.**

A per-fetcher CoverCache tracks bytes and prunes at startup or quota crossing, not every cover. Failed writes do not inflate totals; replacing a file subtracts its previous length. Cached-cover writes use the shared atomic writer. Accounting remains per-loader and approximate under overlapping loader lifetimes.

**Validation:** formatter and Linux cross-target clippy passed. No tests run,
per user instruction; no TV validation.

**Rollback group:** [03-art-and-models.patch](performance-changes/03-art-and-models.patch).
Apply only the whole group; shared files couple findings within it.


The console path calls `store_cover` once per fetched cover. Each call writes the
normalized image and invokes `prune_cache`, which walks directory entries, reads
metadata, and sorts files. With K writes and F cache files, that introduces
approximately K directory walks and K sorts of up to F entries, even when no
eviction is needed. The disk quota bounds F but does not make repeated work free.

The classic art worker already tracks approximate totals and refreshes them when
crossing a budget. Reusing that approach requires an owner spanning all relevant
writers. Two independent totals for the same directory can drift, particularly
when replacing existing entries or when a previous loader is still finishing.
Batch pruning is simpler but should have an explicit maximum overshoot. Temporary
files written by another active worker must not be mistaken for abandoned files.

Measure directory scans, metadata calls, total art-ready time, and bytes over
budget. Test overlapping loader lifetimes, failed writes, and replacing an existing
cover. The goal is fewer filesystem operations with the same durable cache bounds,
not removing quota enforcement.

### Candidate D: Reuse management transport setup

**Implementation status (2026-09-14): Deferred.**

No management transport reuse change; no measured user-facing impact justifies expanding scope.

**Validation / rollback:** no implementation change for this candidate.


Disposition: deferred. No measured user-facing delay is attributed to this work.

The status poll creates an agent through `get_json`, rebuilding its TLS config
and parsing the same certificate/key material. Because the agent is discarded,
the next poll cannot use its connection pool. This is infrequent background work;
it ranks below the unbounded art and lifecycle problems.

A reusable management client can hold the agent and immutable identity/pin
configuration. Request-specific deadlines still belong to individual operations:
an exit action must not inherit a long library request budget. Do not serialize
an urgent operation behind a stalled low-priority request merely to share a
single worker. Connection pooling and work scheduling are separate choices.

Validate credential and pin rotation, host address changes, connection closure
by the server, and differing budgets. Measure TLS handshakes and setup time per
poll interval. Reuse must never preserve an obsolete trust decision after
re-pairing. Actual benefit depends on whether the server keeps connections alive
long enough for the polling interval.

### Candidate E: Shorten pad PCM critical sections

**Implementation status (2026-09-14): Deferred.**

No pad-ring lock cleanup; wait/hold measurements remain the prerequisite.

**Validation / rollback:** no implementation change for this candidate.


Disposition: ignore until acquisition waits or hold durations demonstrate a
problem. Bulk operations may be tidier, but tidiness is not performance evidence.

Per-sample `pop_front` loops and decimation hold their respective PCM-ring mutexes
while performing work proportional to a chunk. Producers need the same rings
to deliver fresh audio. Shortening these sections can reduce producer/consumer
interference, but the chunks are bounded and there is no measured evidence yet
that mutex contention dominates playback.

The straightforward change is the same pattern already used by the SDL ring:
obtain the ring's two contiguous slices, copy the required samples in bulk, then
advance the ring. Preserve sample and channel alignment across the wrap. Where
decimation only needs a local block, copy that bounded block under the lock and
perform arithmetic afterward. Keep any state that must advance atomically with
ring consumption in the same ownership domain.

Instrument acquisition wait and hold time separately, using counters that do not
log while locked. Test wraparound, short buffers, silence fill, and simultaneous
speaker/coil activity. Preserve output samples and lane-activity semantics; a
microbenchmark win alone is insufficient if playback behavior changes.

### Candidate F: Reuse Bluetooth serialization storage

**Implementation status (2026-09-14): Deferred.**

No Bluetooth serialization allocation cleanup; pacing and protocol-sensitive code retained.

**Validation / rollback:** no implementation change for this candidate.


Disposition: deferred cleanup. `payload_for` already reserves capacity with
`String::with_capacity`; the obvious string-growth optimization is present.
Small allocations at this cadence do not establish meaningful CPU or latency
cost, and protocol-sensitive edits need a concrete benefit to justify them.

The paced Bluetooth lane constructs binary reports and textual JSON for LS2.
Repeated vectors, strings, and C-string conversions create allocator traffic at
the report cadence. This is smaller work than image decoding, but it occurs
continuously during pad audio and shares a constrained CPU with the stream.

Separate immutable request fields, such as address and URI, from the report
payload. Reuse capacity for variable payload serialization when the LS2 ownership
contract permits it. Confirm whether the FFI copies request text synchronously
before reusing a buffer; never infer that from the Rust wrapper alone. Borrowing
reply text is safe only within the lifetime guaranteed by the callback.

Measure allocations per report and serialization CPU time. Validate byte-exact
reports, CRC, sequence progression, and valid JSON across all payload values.
Compare audible output and send cadence. Do not batch reports or increase their
rate as part of allocation cleanup: timing is a separate measured constraint.

### Candidate G: Avoid rendering unchanged scenes

**Implementation status (2026-09-14): Deferred beyond finding 7.**

Local disconnect-dialog redraw fix is implemented in finding 7. General shell dirty/deadline API and base-scene/blur caching remain upstream or profiling work.

**Validation / rollback:** no implementation change for this candidate.


**Idle timing correction.** `IDLE_FRAME_STEP` and `TICK_BUDGET` are both 16 ms,
but they are not interchangeable. Let W represent all other iteration time,
including any swap wait. Ignoring scheduler overshoot, the active loop takes
approximately `max(16 ms, W)`. The idle loop sleeps 16 ms unconditionally before
rendering, then only fills the budget if needed: approximately `16 ms + W`.
For 4 ms of other work without blocking swap, that is about 16 ms active versus
20 ms idle. With vsync, the added sleep can change swap wait, so exact cadence
must be measured. The reviewer's claim of zero throttling does not follow from
equal constants. Subtracting idle time from CPU accounting does not erase the
actual sleep.

The confirmed static-dialog issue is local and can be fixed without changing the
shell. Broader redraw elimination needs an explicit account of every animation
and background invalidation source. A shell that appears idle may still animate
focus, loading, image arrival, or transitions. Sleeping longer without observing
those deadlines can create input latency or frozen animations.

For console flow, the clean interface is an upstream request for another frame
or its next deadline. Event arrival should wake the loop immediately. For classic
modal flow, caching the base scene requires invalidation on scroll, art arrival,
host status, display size/scale, and any background animation. A cached full-size
surface consumes GPU memory and can cost more bandwidth than redrawing a simple
scene, so measure both paths.

The glass blur candidate is narrower: record cache hits and misses while opening
and zooming. If rounded sigma changes repeatedly, compare fixed-resolution or
precomputed alternatives visually before changing the material. Preserve the
existing fast path when parameters are unchanged.

Measure CPU preparation, GPU work where observable, and swap wait separately.
A lower CPU timer that merely moves blocking into swap is not proof of faster
rendering. Use container preview for visual correctness, then validate timing on
the TV's actual GPU and compositor.

## Synchronization decisions to preserve

- NDL's global FFI mutex protects undocumented, non-thread-safe vendor calls.
  Do not split it by plane. Instrument wait and hold durations separately before
  changing any scope. Inspected HDR acquisition follows
  `pending_hdr -> applied_hdr -> FFI`; no reverse acquisition was found there.
- Audio timestamp floors must be read and updated under the FFI guard. Moving
  those operations outside it risks a permanent mute, not just timing noise.
- Keep the clock-plane worker separate from the transport audio pump. Their
  waiting behavior differs, and starving the plane breaks video pacing.
- Keep the software jitter policy, 512-frame callback request, and current loss
  recovery without flushing. These choices have device evidence behind them.
- Preserve feedback throttling and Bluetooth pacing. Fix delivery semantics
  without increasing report rates or spawning more subprocesses.
- The state writer correctly releases its queue lock before disk I/O. Its
  problems above concern failure accounting, not disk writes inside that lock.
- No evidence supports wholesale lock-free conversion, extra decoder threads,
  thread-priority boosting, or more aggressive GPU threading.

## Suggested implementation and validation order

1. Fix abandoned connections and USB failure pacing. Exercise cancellation,
   unplug, and repeated failure paths before another performance comparison.
2. Repair feedback delivery and input releases. These prevent persistent
   controller state after transitions.
3. Repair group-aware windows, stale result adoption, and eviction complexity.
   Then make console art demand-driven with bounded pending bytes.
4. Remove repeated static-dialog rendering and bound external art I/O. Handle
   opt-in telemetry and settings-write failure accounting as lower-priority
   robustness fixes.
5. Profile callback tails, model rebuilds, cache I/O, and upstream shell frames.
   Pursue substantial additional changes only where measurements justify them;
   bounded callback preallocation can be done opportunistically.

After implementation, run `task docker:lint` and `task docker:test`. A macOS
`cargo check` excludes the relevant Linux modules and is insufficient. Keep
algorithmic tests executable on a host runner; avoid armv7-only tests that only
compile. Use the container preview for UI correctness, not TV timing.

On CX/G5, compare identical release builds and workloads. Record UI frame
p50/p90/p99, peak RSS and decoded bytes, visible-cover latency, audio callback
tails and underruns, input delivery gaps, NDL lock wait/hold times, and live worker
counts after cancellation. Retain the existing pacing, plane-lead, and audio
target counters to detect regressions while reducing unrelated work.
