//! The binary side of the console: the models the shell reads, and the commands it raises back.
//!
//! The shell never touches the network or the disk — it writes a [`ConsoleCmd`] and reads a
//! snapshot. Everything on this side either answers one of those commands or keeps the home
//! carousel's rows current, and all of it runs on the menu thread's tick except the blocking
//! parts, which go to a worker exactly as the desktop's `clients/session/src/console.rs` does.
//!
//! What this client cannot answer it says so about rather than faking: it has no per-host
//! clipboard flag and no platform-native screens, so those commands log and (where the shell
//! is waiting on something) post a notice.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use pf_console_ui::{
    ConsoleCmd, ConsoleHandles, HostAction, HostRow, LibraryGame, LibraryPhase, PairPhase, Stale, WakeStatus,
};

use crate::core::model::{GameEntry, KnownHost};
use crate::services::discovery::{DiscoveredHost, Discovery};
use crate::services::library::{self, GamesLoaded, LibraryError, DEFAULT_MGMT_PORT};
use crate::services::power::{self, PowerRights};
use crate::services::store::console::ConsoleStore;
use crate::services::store::{self, shared, ExitAction};
use crate::services::{budget, wol};

/// How often every row is re-probed for its presence pip. Ten seconds is the desktop's
/// cadence; this `SoC` probes sequentially on one thread rather than in parallel, so the sweep
/// itself is the slow part, not the interval.
const SWEEP_EVERY: Duration = Duration::from_secs(10);

/// How often [`Service::tick`] does its work. The desktop's service loop runs at this cadence on
/// its own thread; here it shares the render loop, and rebuilding the rows means cloning the
/// document and allocating a `Vec<HostRow>` — sixty times a second on a three-core TV, for a
/// list that only changes when mDNS says so. Input is still handled every frame.
const SERVICE_EVERY: Duration = Duration::from_millis(100);

/// The wake loop's budget and cadence — the desktop's, so a host woken from the TV and from a
/// laptop give up at the same moment.
const WAKE_TIMEOUT: Duration = Duration::from_secs(90);
const WAKE_RESEND_EVERY: Duration = Duration::from_secs(6);

/// What a finished pairing ceremony reports. Mirrors `app::PairingOutcome`, minus the fields
/// only the old UI's host list needs — the shell re-reads its rows from the document.
struct PairOutcome {
    name: String,
    addr: String,
    port: u16,
    mgmt_port: Option<u16>,
    mac: Vec<String>,
    os: String,
    result: Result<[u8; 32], String>,
}

/// A catalog profile as the shell's chip.
fn chip(p: &pf_client_core::profiles::StreamProfile) -> pf_console_ui::ProfileChip {
    pf_console_ui::ProfileChip {
        id: p.id.clone(),
        name: p.name.clone(),
        accent: p.accent.clone(),
    }
}

/// One host the sweep will ask about. A struct rather than a tuple because five fields in a
/// row is exactly where positional access stops being readable.
struct SweepTarget {
    key: String,
    addr: String,
    port: u16,
    mgmt: u16,
    fingerprint: Option<[u8; 32]>,
}

/// One host's sweep answer: is it up, and (for a paired one) what it lets this TV do to it.
struct Swept {
    key: String,
    online: bool,
    rights: Option<PowerRights>,
}

pub(crate) struct Service {
    handles: ConsoleHandles,
    pub(crate) store: Arc<ConsoleStore>,
    identity: (String, String),
    /// `None` if the mDNS daemon would not start — the carousel then shows saved hosts only.
    discovery: Option<Discovery>,
    /// Live adverts, one per `addr:port`.
    discovered: Vec<DiscoveredHost>,
    /// Row key → last probe answer, and what that host offers. Both survive a sweep so a pip
    /// does not blink off while the next round runs.
    reachable: HashMap<String, bool>,
    rights: HashMap<String, PowerRights>,
    sweep: Option<Receiver<Swept>>,
    last_sweep: Option<Instant>,
    /// When [`Self::tick`] last did its work — see [`SERVICE_EVERY`].
    last_tick: Option<Instant>,
    games: Option<Receiver<GamesLoaded>>,
    /// Covers as they arrive, decoded on the fetch thread where possible — see [`spawn_art`].
    /// Deliberately NOT through `services::art`, which decodes to a card-sized pixmap for the
    /// old UI's tiny-skia compositor.
    art: Option<Receiver<(String, ArtItem)>>,
    pair: Option<Receiver<PairOutcome>>,
    /// Set for the wake worker to see; taking it is how a cancel or a second wake stops it.
    wake_cancel: Option<Arc<AtomicBool>>,
}

impl Service {
    pub(crate) fn new(handles: ConsoleHandles, store: Arc<ConsoleStore>, identity: (String, String)) -> Self {
        Self {
            handles,
            store,
            identity,
            discovery: Discovery::start(),
            discovered: Vec::new(),
            reachable: HashMap::new(),
            rights: HashMap::new(),
            sweep: None,
            last_sweep: None,
            last_tick: None,
            games: None,
            art: None,
            pair: None,
            wake_cancel: None,
        }
    }

    /// One menu tick: fold in whatever the background answered, start a sweep if one is due,
    /// serve the shell's commands, and publish the rows.
    pub(crate) fn tick(&mut self) {
        // Commands are the one thing that must not wait for the cadence: they are a button
        // press, and the shell shows nothing until one is served.
        for cmd in self.handles.bus.drain() {
            self.handle(cmd);
        }
        if self.last_tick.is_some_and(|t| t.elapsed() < SERVICE_EVERY) {
            return;
        }
        self.last_tick = Some(Instant::now());
        if let Some(discovery) = &mut self.discovery {
            for host in discovery.poll() {
                self.discovered
                    .retain(|d| !(d.addr == host.addr && d.port == host.port));
                self.discovered.push(host);
            }
        }
        self.drain_sweep();
        self.drain_games();
        self.drain_art();
        self.drain_pair();
        if self.last_sweep.is_none_or(|t| t.elapsed() >= SWEEP_EVERY) {
            self.start_sweep();
        }
        // `set_hosts` bumps its generation only on a real change, so the shell redraws when the
        // list moves rather than on this cadence.
        self.handles.console.set_hosts(self.rows());
    }

    /// Stop everything this service started. The wake worker is the only one that would
    /// outlive the menu — the rest end when their receiver drops with `self`.
    pub(crate) fn stop(&mut self) {
        if let Some(cancel) = self.wake_cancel.take() {
            cancel.store(true, Ordering::SeqCst);
        }
    }

    // ---- the models the shell reads ---------------------------------------------------

    /// The home carousel: saved hosts (most recently used first), each followed by its pinned
    /// profile cards, then discovered-but-unsaved ones. A pinned card shares its host's live
    /// state; its key rides the profile id behind a NUL, as the desktop's does.
    fn rows(&self) -> Vec<HostRow> {
        let state = self.store.snapshot();
        let catalog = pf_client_core::profiles::ProfilesFile {
            version: pf_client_core::profiles::PROFILES_VERSION,
            profiles: state.profiles.clone(),
        };
        let mut hosts: Vec<(HostRow, Vec<HostRow>)> = state
            .known_hosts
            .iter()
            .map(|h| {
                let mut row = self.saved_row(h);
                row.bound_profile = h.profile_id.as_deref().and_then(|id| catalog.find_by_id(id)).map(chip);
                let pins = h
                    .resolved_pins(&catalog)
                    .into_iter()
                    .map(|p| HostRow {
                        key: format!("{}\0{}", row.key, p.id),
                        pin: Some(chip(p)),
                        bound_profile: None,
                        ..row.clone()
                    })
                    .collect();
                (row, pins)
            })
            .collect();
        hosts.sort_by(|(a, _), (b, _)| b.last_used.cmp(&a.last_used).then_with(|| a.name.cmp(&b.name)));
        let mut saved: Vec<HostRow> = hosts
            .into_iter()
            .flat_map(|(row, pins)| std::iter::once(row).chain(pins))
            .collect();
        let mut extra: Vec<HostRow> = self
            .discovered
            .iter()
            .filter(|d| !state.known_hosts.iter().any(|h| same_host(h, d)))
            .map(|d| HostRow {
                key: shared::host_key("", &d.addr, d.port),
                name: d.name.clone(),
                addr: d.addr.clone(),
                port: d.port,
                fp_hex: String::new(),
                paired: false,
                saved: false,
                // It is answering mDNS right now, which is the whole of what "online" claims.
                online: true,
                mgmt_port: d.mgmt_port.unwrap_or(DEFAULT_MGMT_PORT),
                can_wake: false,
                clipboard_sync: false,
                last_used: None,
                os: d.os.clone(),
                // Unpaired: there is nothing it would let this TV do to it.
                actions: Vec::new(),
                pin: None,
                bound_profile: None,
                game_profiles: Default::default(),
                // Needs `/api/v1/status`, which this client does not ask — the same reason
                // `LibraryGame::running` is false here. Empty renders as no line.
                running: String::new(),
            })
            .collect();
        extra.sort_by(|a, b| a.name.cmp(&b.name));
        saved.extend(extra);
        saved
    }

    /// The row for the host the document last had selected, if it is still known and paired —
    /// what the console enters on, mirroring the classic menus' own restore.
    pub(crate) fn selected_row(&self) -> Option<HostRow> {
        let state = self.store.snapshot();
        let (host, port) = state.selected_host.clone()?;
        let known = state
            .known_hosts
            .iter()
            .find(|h| h.addr == host && h.port == port && h.is_paired())?;
        Some(self.saved_row(known))
    }

    fn saved_row(&self, h: &KnownHost) -> HostRow {
        let key = shared::known_host_key(h);
        let advert = self.discovered.iter().find(|d| same_host(h, d));
        let online = advert.is_some() || self.reachable.get(&key).copied().unwrap_or(false);
        HostRow {
            name: if h.name.is_empty() {
                h.addr.clone()
            } else {
                h.name.clone()
            },
            addr: h.addr.clone(),
            port: h.port,
            fp_hex: h.fp_hex.clone(),
            paired: h.is_paired(),
            saved: true,
            online,
            // The live advert first, then what was saved from an earlier one: reading only the
            // advert is how a host on a moved management port loses its library the moment
            // mDNS goes quiet.
            mgmt_port: advert
                .and_then(|d| d.mgmt_port)
                .or(h.mgmt_port)
                .unwrap_or(DEFAULT_MGMT_PORT),
            can_wake: !online && !h.mac.is_empty(),
            // This client has no per-host clipboard flag; see `ConsoleCmd::SetClipboard`.
            clipboard_sync: false,
            // No launch history on the record — `services::recents` keys per host and game,
            // and ordering by name is honest rather than pretending to a recency it lacks.
            last_used: None,
            os: advert
                .filter(|d| !d.os.is_empty())
                .map_or_else(|| h.os.clone(), |d| d.os.clone()),
            actions: self.rights.get(&key).copied().map(power_rows).unwrap_or_default(),
            pin: None,
            bound_profile: None,
            // Title id → profile id, straight off the record: the bind screen only compares
            // these against the catalog it was handed, and a dangling one resolves to nothing
            // there exactly as it does at launch.
            game_profiles: h.game_profiles.clone(),
            running: String::new(),
            key,
        }
    }

    // ---- the command bus ---------------------------------------------------------------

    fn handle(&mut self, cmd: ConsoleCmd) {
        match cmd {
            ConsoleCmd::FetchLibrary { addr, mgmt, fp_hex } => self.fetch_library(&addr, mgmt, &fp_hex),
            ConsoleCmd::Pair {
                addr,
                port,
                pin,
                device_name,
            } => self.start_pair(addr, port, &pin, &device_name),
            ConsoleCmd::SendLogs { host_name, .. } => {
                // The shell's row means "upload to that host's management API". This client
                // uploads to the developer's endpoint instead (`app::state::sendlogs`), which
                // is a different destination with different consent — so it says what it can
                // do rather than quietly doing the other thing.
                self.notice(format!(
                    "This TV can't send logs to {host_name} yet — use Diagnostics ▸ Send logs to developer"
                ));
            }
            ConsoleCmd::HostAction {
                addr,
                mgmt,
                fp_hex,
                host_name,
                action_id,
                label,
            } => self.host_action(addr, mgmt, &fp_hex, host_name, action_id, label),
            ConsoleCmd::SaveHost { name, addr, port } => self.save_host(name, addr, port),
            ConsoleCmd::UpdateHost { key, name, addr, port } => self.update_host(&key, name, addr, port),
            ConsoleCmd::ForgetHost { key } => self.forget_host(&key),
            ConsoleCmd::Wake { key, then_connect } => self.start_wake(&key, then_connect),
            ConsoleCmd::CancelWake => {
                self.stop();
                self.handles.console.set_wake(None);
            }
            ConsoleCmd::Probe => self.start_sweep(),
            // Nothing this client draws: it has no licences screen of its own, and the pad
            // grants and rumble tests are Android's `InputDevice` API.
            ConsoleCmd::OpenPlatformScreen { id } => tracing::info!("console: no platform screen {id} on webOS"),
            ConsoleCmd::PadAction { action, .. } => tracing::info!("console: no pad action {action} on webOS"),
            // Bind (or clear) one title's profile — the shell's "Settings profile…" row on a
            // cover. The host half of the key is what addresses the record; the catalog itself
            // is only ever written by the per-game screen, so an id naming nothing is refused
            // rather than stored.
            ConsoleCmd::BindProfile {
                key,
                game: Some(game),
                profile_id,
            } => self.bind_game_profile(&key, &game, profile_id.as_deref()),
            // The host's own default binding (`KnownHost::profile_id`); `None` clears it.
            ConsoleCmd::BindProfile {
                key,
                game: None,
                profile_id,
            } => self.bind_host_profile(&key, profile_id),
            // Presentation only: which profiles ride as cards behind the host's tile.
            ConsoleCmd::SetPin { key, profile_id, pin } => self.set_pin(&key, profile_id, pin),
            // Two commands with nothing to do here, each for its own reason:
            // - `RefreshRunning`: no `/api/v1/status` client, so the running set stays empty
            //   and every Resume badge stays off — exactly how the shell draws a host too old
            //   to answer it. Wiring one needs the status shape, not just another request.
            // - `SetClipboard`: `KnownHost` carries no clipboard flag and the stream has no
            //   clipboard lane to gate, so the toggle would be a control that does nothing.
            ConsoleCmd::RefreshRunning { .. } | ConsoleCmd::SetClipboard { .. } => {}
        }
    }

    /// Point one title at a catalog profile, or clear it. Refuses an id the catalog does not
    /// hold: the record must never name a profile nothing resolves, and the shell can only
    /// offer ids it was handed, so one that misses means the two went out of step.
    fn bind_game_profile(&self, key: &str, game: &str, profile_id: Option<&str>) {
        let changed = self.store.edit(|state| {
            if let Some(id) = profile_id {
                if !state.profiles.iter().any(|p| p.id == id) {
                    tracing::warn!(%id, "console: bind to a profile this document does not hold");
                    return false;
                }
            }
            let Some(i) = shared::find_known(&state.known_hosts, key) else {
                tracing::warn!(%key, "console: profile bind for an unknown host");
                return false;
            };
            let host = &mut state.known_hosts[i];
            if host.game_profile(game) == profile_id {
                return false;
            }
            host.bind_game_profile(game, profile_id);
            true
        });
        if changed {
            self.handles.console.set_hosts(self.rows());
        }
    }

    /// The host's default binding, through the shared edit; the carousel re-reads on a change.
    fn bind_host_profile(&self, key: &str, profile_id: Option<String>) {
        if self
            .store
            .edit(|state| shared::bind_host_profile(state, key, profile_id))
        {
            self.handles.console.set_hosts(self.rows());
        }
    }

    /// A pinned card on or off, through the shared edit; the carousel re-reads on a change.
    fn set_pin(&self, key: &str, profile_id: String, pin: bool) {
        if self.store.edit(|state| shared::set_pin(state, key, profile_id, pin)) {
            self.handles.console.set_hosts(self.rows());
        }
    }

    fn notice(&self, text: String) {
        self.handles.console.set_notice(text);
    }

    // ---- library -----------------------------------------------------------------------

    fn fetch_library(&mut self, addr: &str, mgmt: u16, fp_hex: &str) {
        // `begin_fetch` rather than a bare Loading phase: it also advances the fetch epoch,
        // which is how the shelf tells its own result from the previous host's.
        self.handles.library.begin_fetch();
        // Dropping the old receivers cancels them — a result for the host we just navigated
        // away from must not land on this shelf.
        self.art = None;
        // `FetchLibrary` names the management port, not the stream one, so the record supplies
        // it — only so the answer can say which host proved reachable.
        let port = self
            .store
            .snapshot()
            .known_hosts
            .iter()
            .find(|h| h.addr == addr)
            .map_or(0, |h| h.port);
        self.games = Some(library::load_games_async(
            addr.to_string(),
            port,
            mgmt,
            self.identity.clone(),
            shared::parse_fp(fp_hex),
            budget::REQUEST,
        ));
    }

    fn drain_games(&mut self) {
        let Some(rx) = &self.games else { return };
        let Ok(loaded) = rx.try_recv() else { return };
        self.games = None;
        let games = match loaded.result {
            Ok(games) => games,
            Err(e) => {
                tracing::warn!("console: library fetch failed: {e}");
                // A transport failure is the one worth retrying; a rejected certificate does
                // not become acceptable by asking again.
                let can_retry = matches!(e, LibraryError::Unreachable(_));
                self.handles.library.set_phase(LibraryPhase::Error {
                    title: "Couldn't load the library".into(),
                    body: e.to_string(),
                    can_retry,
                });
                return;
            }
        };
        self.note_reachable(&loaded.host, loaded.port, true);
        self.handles.library.set_games(to_model(&games));
        self.handles.library.set_stale(Stale::No);
        self.art = Some(spawn_art(
            loaded.host,
            loaded.mgmt_port,
            loaded.port,
            self.identity.clone(),
            games,
            self.handles.library.clone(),
        ));
    }

    fn drain_art(&mut self) {
        let Some(rx) = &self.art else { return };
        // Bounded per tick: a warm host answers faster than the panel refreshes, and draining
        // the whole channel here would hold the frame for as long as art keeps arriving.
        for _ in 0..8 {
            let Ok((id, item)) = rx.try_recv() else { return };
            match item {
                ArtItem::Decoded(poster) => self.handles.library.push_decoded(id, poster),
                ArtItem::Encoded(bytes) => self.handles.library.push_art(id, bytes),
            }
        }
    }

    // ---- pairing -----------------------------------------------------------------------

    fn start_pair(&mut self, addr: String, port: u16, pin: &str, device_name: &str) {
        let name = self
            .rows()
            .into_iter()
            .find(|r| r.addr == addr && r.port == port)
            .map_or_else(|| addr.clone(), |r| r.name);
        let advert = self.discovered.iter().find(|d| d.addr == addr && d.port == port);
        let (mgmt_port, mac, os) = advert.map_or_else(
            || (None, Vec::new(), String::new()),
            |d| (d.mgmt_port, d.mac.clone(), d.os.clone()),
        );
        self.handles.console.set_pair(PairPhase::Busy);
        let (tx, rx) = std::sync::mpsc::channel();
        self.pair = Some(rx);
        let identity = self.identity.clone();
        let (pin, device_name) = (pin.to_string(), device_name.to_string());
        std::thread::Builder::new()
            .name("punktfunk-webos-console-pair".into())
            .spawn(move || {
                let result = punktfunk_core::client::NativeClient::pair(
                    &addr,
                    port,
                    (&identity.0, &identity.1),
                    &pin,
                    &device_name,
                    // Gated on somebody walking to their PC, so the long host budget.
                    budget::HOST_WAIT,
                )
                .map_err(|e| crate::core::errors::pair_message(&e));
                // A failed send just means the ceremony was cancelled and nobody is listening.
                let _ = tx.send(PairOutcome {
                    name,
                    addr,
                    port,
                    mgmt_port,
                    mac,
                    os,
                    result,
                });
            })
            .ok();
    }

    fn drain_pair(&mut self) {
        let Some(rx) = &self.pair else { return };
        let Ok(outcome) = rx.try_recv() else { return };
        self.pair = None;
        let fingerprint = match outcome.result {
            Ok(fp) => fp,
            Err(e) => {
                tracing::warn!("console: pairing failed: {e}");
                self.handles.console.set_pair(PairPhase::Failed(e));
                return;
            }
        };
        tracing::info!("console: paired with {}:{}", outcome.addr, outcome.port);
        let key = shared::host_key(&shared::hex(fingerprint), &outcome.addr, outcome.port);
        self.store.edit(|state| {
            // Only reaches a genuinely new host — `upsert_known_host` keeps an existing
            // record's bindings and collections.
            let mut record = KnownHost {
                shared: pf_client_core::trust::KnownHost {
                    name: outcome.name,
                    addr: outcome.addr,
                    port: outcome.port,
                    mgmt_port: outcome.mgmt_port,
                    mac: outcome.mac,
                    os: outcome.os,
                    ..Default::default()
                },
                ..KnownHost::default()
            };
            record.set_fingerprint(fingerprint);
            store::upsert_known_host(&mut state.known_hosts, record);
            true
        });
        self.handles.console.set_pair(PairPhase::Paired { key });
        // It answered a handshake a moment ago, so the pip should not wait for the next sweep.
        self.last_sweep = None;
    }

    // ---- the host list -----------------------------------------------------------------

    fn save_host(&mut self, name: String, addr: String, port: u16) {
        self.store.edit(|state| {
            // Keyed by address, not fingerprint: a hand-typed host has none yet, so an
            // fp-keyed upsert would collide every one of them onto the same record.
            if let Some(h) = state.known_hosts.iter_mut().find(|h| h.addr == addr && h.port == port) {
                if !name.is_empty() {
                    h.name = name;
                }
            } else {
                state.known_hosts.push(KnownHost {
                    shared: pf_client_core::trust::KnownHost {
                        name: if name.is_empty() { addr.clone() } else { name },
                        addr,
                        port,
                        ..Default::default()
                    },
                    ..KnownHost::default()
                });
            }
            true
        });
        self.last_sweep = None;
    }

    fn update_host(&mut self, key: &str, name: String, addr: String, port: u16) {
        let edited = self.store.edit(|state| {
            let Some(i) = shared::find_known(&state.known_hosts, key) else {
                return false;
            };
            // Edited in place rather than removed and re-added: the fingerprint, the learned
            // MAC and every per-game override hang off this record, and re-adding would
            // silently unpair a host somebody only renamed.
            let h = &mut state.known_hosts[i];
            h.name = if name.trim().is_empty() { addr.clone() } else { name };
            h.addr = addr;
            h.port = port;
            true
        });
        if edited {
            // The address may have moved.
            self.last_sweep = None;
        } else {
            tracing::warn!("console: edit for an unknown host ({key}) — ignoring");
        }
    }

    fn forget_host(&mut self, key: &str) {
        let mut gone: Option<KnownHost> = None;
        self.store.edit(|state| {
            let Some(i) = shared::find_known(&state.known_hosts, key) else {
                return false;
            };
            gone = Some(state.known_hosts.remove(i));
            // A host the list no longer shows must not stay selected.
            if state
                .selected_host
                .as_ref()
                .is_some_and(|(h, p)| gone.as_ref().is_some_and(|g| g.addr == *h && g.port == *p))
            {
                state.selected_host = None;
            }
            true
        });
        let Some(gone) = gone else {
            tracing::warn!("console: forget for an unknown host ({key}) — ignoring");
            return;
        };
        tracing::info!("console: forgot {} ({}:{})", gone.name, gone.addr, gone.port);
        // Its covers are keyed by host and would otherwise outlive the record. This is the
        // last moment the address is known.
        crate::services::art::reconcile_host_caches(&self.store.snapshot().known_hosts);
        // It may still be advertising, in which case it comes straight back as a discovered
        // row — unsaved and unpaired, which is the honest state.
        self.last_sweep = None;
    }

    // ---- reachability, rights, wake ------------------------------------------------------

    fn start_sweep(&mut self) {
        if self.sweep.is_some() {
            return;
        }
        let state = self.store.snapshot();
        let targets: Vec<SweepTarget> = state
            .known_hosts
            .iter()
            .map(|h| SweepTarget {
                key: shared::known_host_key(h),
                addr: h.addr.clone(),
                port: h.port,
                mgmt: h.mgmt_port.unwrap_or(DEFAULT_MGMT_PORT),
                fingerprint: h.fingerprint(),
            })
            .collect();
        if targets.is_empty() {
            return;
        }
        self.last_sweep = Some(Instant::now());
        let (tx, rx) = std::sync::mpsc::channel();
        self.sweep = Some(rx);
        let identity = self.identity.clone();
        // One thread for the whole sweep, probing sequentially: the host count is a handful,
        // and a thread per host would spike this SoC's three cores for a presence dot.
        std::thread::Builder::new()
            .name("punktfunk-webos-console-sweep".into())
            .spawn(move || {
                for t in targets {
                    let online = punktfunk_core::client::NativeClient::probe(&t.addr, t.port, budget::PROBE);
                    // Only a paired host that answered has anything to say about power, and
                    // only that pairing's own access mask decides it.
                    let rights = (online && t.fingerprint.is_some())
                        .then(|| power::probe_rights(&t.addr, t.mgmt, &identity, t.fingerprint).ok())
                        .flatten();
                    if tx
                        .send(Swept {
                            key: t.key,
                            online,
                            rights,
                        })
                        .is_err()
                    {
                        return;
                    }
                }
            })
            .ok();
    }

    fn drain_sweep(&mut self) {
        let Some(rx) = &self.sweep else { return };
        let mut done = false;
        loop {
            match rx.try_recv() {
                Ok(s) => {
                    self.reachable.insert(s.key.clone(), s.online);
                    match s.rights {
                        Some(r) => {
                            self.rights.insert(s.key, r);
                        }
                        None => {
                            self.rights.remove(&s.key);
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        if done {
            self.sweep = None;
        }
    }

    /// Whether this host answered its last check. Unknown counts as down, the same rule the
    /// classic menus' exit path reads — a host that has never been probed must not cost the
    /// exit budget on a connection that cannot complete.
    pub(crate) fn is_online(&self, h: &KnownHost) -> bool {
        self.discovered.iter().any(|d| same_host(h, d))
            || self.reachable.get(&shared::known_host_key(h)).copied().unwrap_or(false)
    }

    /// Record what some other exchange already proved about a host, so the pip never sits
    /// behind evidence the app has in hand.
    fn note_reachable(&mut self, addr: &str, port: u16, online: bool) {
        let state = self.store.snapshot();
        let key = state
            .known_hosts
            .iter()
            .find(|h| h.addr == addr && h.port == port)
            .map_or_else(|| shared::host_key("", addr, port), shared::known_host_key);
        self.reachable.insert(key, online);
    }

    fn start_wake(&mut self, key: &str, then_connect: bool) {
        self.stop();
        let Some(row) = self.rows().into_iter().find(|r| r.key == key) else {
            return;
        };
        let state = self.store.snapshot();
        let macs = shared::find_known(&state.known_hosts, key)
            .map(|i| state.known_hosts[i].mac.clone())
            .unwrap_or_default();
        if macs.is_empty() {
            self.notice(format!("{} has no wake address on record", row.name));
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        self.wake_cancel = Some(cancel.clone());
        spawn_wake(self.handles.clone(), row, macs, then_connect, cancel);
    }

    fn host_action(&self, addr: String, mgmt: u16, fp_hex: &str, host_name: String, action_id: String, label: String) {
        let identity = self.identity.clone();
        let pin = shared::parse_fp(fp_hex);
        let console = self.handles.console.clone();
        // A 202 is the last word: the host ends every session and acts a second later, so
        // there is nothing to poll and nothing to undo.
        std::thread::Builder::new()
            .name("punktfunk-webos-console-hostaction".into())
            .spawn(
                move || match power::invoke(&addr, mgmt, &identity, pin, &action_id, budget::REQUEST) {
                    Ok(()) => {
                        tracing::info!("console: {host_name} accepted {action_id}");
                        console.set_notice(format!("{host_name}: {label} — on its way"));
                    }
                    Err(e) => {
                        tracing::warn!("console: {host_name} refused {action_id}: {e}");
                        console.set_notice(format!("{label} failed — {e}"));
                    }
                },
            )
            .ok();
    }
}

/// Whether a saved record and a live advert are the same host.
///
/// A function rather than the comparison written out at each of its three call sites: the two
/// types carry the address under different names (`KnownHost::host`, `DiscoveredHost::addr`),
/// and inline that mismatch reads as a copy-paste slip to a human and to clippy alike.
fn same_host(h: &KnownHost, d: &DiscoveredHost) -> bool {
    // Compared as pairs, not as two `&&`-ed equalities: with the address fields spelled
    // differently on the two types, that shape reads as a typo to `suspicious_operation_groupings`.
    (h.addr.as_str(), h.port) == (d.addr.as_str(), d.port)
}

/// The two power rows this client renders, from the rights the host reported.
///
/// Built locally rather than passed through from the host's own `/api/v1/actions`, because
/// `services::power` trims that reply to the two flags it acts on — this client has never
/// rendered a host-named action generically, and inventing labels it did not read would be
/// worse than naming the two it does understand.
fn power_rows(rights: PowerRights) -> Vec<HostAction> {
    [
        (ExitAction::Sleep, "Sleep", false),
        (ExitAction::Shutdown, "Shut down", true),
    ]
    .into_iter()
    .filter_map(|(action, label, danger)| {
        let id = action.action_id()?;
        rights.allows(action).then(|| HostAction {
            id: id.to_string(),
            label: label.to_string(),
            danger,
            available: true,
            unavailable_reason: String::new(),
        })
    })
    .collect()
}

/// The wake-and-wait loop: re-send the magic packet every 6 s, probe once a second, give up at
/// 90 s. The thread owns the wake card; the shell reads `online`/`timed_out` and acts.
fn spawn_wake(handles: ConsoleHandles, row: HostRow, macs: Vec<String>, then_connect: bool, cancel: Arc<AtomicBool>) {
    std::thread::Builder::new()
        .name("punktfunk-webos-console-wake".into())
        .spawn(move || {
            let last_ip = row.addr.parse::<Ipv4Addr>().ok();
            let started = Instant::now();
            let mut last_packet: Option<Instant> = None;
            loop {
                if cancel.load(Ordering::SeqCst) {
                    handles.console.set_wake(None);
                    return;
                }
                let elapsed = started.elapsed();
                let timed_out = elapsed >= WAKE_TIMEOUT;
                if !timed_out && last_packet.is_none_or(|t| t.elapsed() >= WAKE_RESEND_EVERY) {
                    wol::wake_and_log(&macs, last_ip, &row.name);
                    last_packet = Some(Instant::now());
                }
                let online = punktfunk_core::client::NativeClient::probe(&row.addr, row.port, budget::PROBE);
                handles.console.set_wake(Some(WakeStatus {
                    key: row.key.clone(),
                    name: row.name.clone(),
                    seconds: elapsed.as_secs() as u32,
                    timed_out,
                    online,
                    then_connect,
                }));
                if online || timed_out {
                    // Awake → the shell connects and cancels; timed out → the card waits for
                    // Try Again, which spawns a fresh one. Either way this thread is done.
                    return;
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        })
        .ok();
}

/// Fetch every title's poster in the background, encoded. One agent for the whole run, so the
/// covers cost one mTLS handshake rather than one each.
/// A cover on its way to the shelf: decoded here where there is a thread to spare, or still
/// encoded when this one could not do it.
enum ArtItem {
    Decoded(pf_console_ui::DecodedPoster),
    Encoded(Vec<u8>),
}

/// Fetch every game's cover, and decode it here rather than on the thread that draws.
///
/// A full-size PNG cover costs ~90 ms on a CX — five frames — and the shelf used to stop for
/// each one. This thread is already waiting on the network, so the work lands where nothing is
/// watching. The shelf publishes the size it caches at; before it has drawn once there is no
/// size to decode to, and those few covers go over encoded as they always did.
///
/// Disk first (`services::art`), which is the same cache the classic menus fill: leaving a
/// shelf and coming back re-asks for every cover, and over Wi-Fi that was ~10 MB and the whole
/// reason returning to the host list felt slow.
fn spawn_art(
    addr: String,
    mgmt: u16,
    port: u16,
    identity: (String, String),
    games: Vec<GameEntry>,
    library: pf_console_ui::LibraryShared,
) -> Receiver<(String, ArtItem)> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("punktfunk-webos-console-art".into())
        .spawn(move || {
            let pin = None;
            let Ok(agent) = library::agent(&identity, pin) else {
                return;
            };
            for game in games {
                let hand_over = |bytes: Vec<u8>| {
                    let item = library
                        .art_scale()
                        .and_then(|k| pf_console_ui::decode_poster_off_thread(&bytes, k))
                        .map_or(ArtItem::Encoded(bytes), ArtItem::Decoded);
                    tx.send((game.id.clone(), item))
                };
                if let Some(bytes) = crate::services::art::cached_cover(&addr, port, &game.id) {
                    // A closed channel means the shelf moved on; stop fetching for it.
                    if hand_over(bytes).is_err() {
                        return;
                    }
                    continue;
                }
                // Portrait first (the card's aspect), then the wider art as a fallback — the
                // same order `services::art` asks in for the old UI's covers.
                let candidates = [&game.art.portrait, &game.art.header, &game.art.hero];
                for path in candidates.into_iter().flatten() {
                    match library::fetch_art(&agent, &addr, mgmt, path) {
                        Ok(bytes) => {
                            crate::services::art::store_cover(&addr, port, &game.id, &bytes);
                            if hand_over(bytes).is_err() {
                                return;
                            }
                            break;
                        }
                        Err(e) => tracing::debug!("console: art {path} for {}: {e}", game.id),
                    }
                }
            }
        })
        .ok();
    rx
}

/// The wire catalog in the shell's terms.
///
/// This client's `GameEntry` is thinner than the desktop's: no launcher role and no platform
/// string, so both are reported as unknown rather than guessed. `store` comes off the id,
/// which is store-qualified by contract (see [`shared::store_of`]).
fn to_model(games: &[GameEntry]) -> Vec<LibraryGame> {
    games
        .iter()
        .map(|g| LibraryGame {
            id: g.id.clone(),
            title: g.title.clone(),
            store: shared::store_of(&g.id).to_string(),
            launcher: false,
            icon: g.icon.clone().unwrap_or_default(),
            platform: None,
            // Catalog detail the desktop's `GameEntry` carries and this client's does not, so
            // it is reported absent rather than guessed — the same rule as `launcher` above.
            developer: None,
            year: None,
            genres: Vec::new(),
            // Host state, and this client never asks for it — see `ConsoleCmd::RefreshRunning`.
            running: false,
        })
        .collect()
}
