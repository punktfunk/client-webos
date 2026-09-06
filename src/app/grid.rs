//! The game grid: its layout vocabulary, its tuning, and the state the render path keeps for it.
//!
//! [`GridLayout`] is the shape a given column count gives the library's [`Group`]s — pure
//! arithmetic over a bounded run list, so the pointer path and the focus map can both ask it
//! without a rasterizer. [`GridState`] is what `App` holds across frames: which tile each card
//! owns, the pop clocks, the eased scroll, and the dirty flags the event side sets for the
//! render side.
//!
//! Pixel geometry (card rects, the visible band) is `app::view::home`; navigation is
//! `app::state::home`; the groups themselves are built in `app::library`.
use std::ops::Range;
use std::time::Instant;

use crate::app::spinner::GridReveal;
use crate::app::view::home::{SECTION_GAP, SECTION_HEADING_H};
use crate::core::model::GameEntry;

/// Prefetch rows beyond viewport (prevents stalls).
pub(crate) const CARD_PREFETCH_ROWS: i32 = 2;
/// Rows beyond which tiles are dropped (hysteresis prevents oscillation).
pub(crate) const CARD_KEEP_ROWS: i32 = 5;
/// One grid section: a collection's cards, laid out as a run of whole rows. Built once per
/// change by [`Library::regroup`](crate::app::library::Library::regroup) and read by every
/// geometry path; deliberately column-independent, so a width change needs no rebuild.
pub(crate) struct Group {
    /// The collection's name, as the section heading draws it.
    pub name: String,
    /// Cards in it.
    pub len: usize,
    /// Index into `Library::games` of this group's first game — the games are laid out
    /// group by group, so each group's are one contiguous run.
    pub games_start: usize,
}

/// A [`Group`] with the geometry `columns` gives it. Yielded by [`GridLayout::placed`]; never
/// stored, so a column change cannot leave a stale copy behind.
#[derive(Clone, Copy)]
pub(crate) struct Placed<'a> {
    pub group: &'a Group,
    /// First grid slot. Whole rows: a partial last row pads rather than letting the next
    /// group's first card share it.
    pub first_idx: usize,
    pub first_row: usize,
    pub rows: usize,
    /// Heading and gap pixels stacked above this group's first row.
    pub y_offset: i32,
}

impl Placed<'_> {
    /// The slots this group actually fills — [`Self::first_idx`] plus its cards, so the
    /// padding at the end of a partial last row is outside it.
    pub(crate) fn slots(&self) -> Range<usize> {
        self.first_idx..self.first_idx + self.group.len
    }

    fn rows_range(&self) -> Range<usize> {
        self.first_row..self.first_row + self.rows
    }
}

/// The grid's shape at one column count: the library's groups plus the arithmetic that turns
/// them into slots, rows and vertical offsets.
///
/// `Copy` and borrowing only the group list, so a caller can hold one and still mutate the
/// rest of `App` — which is why the accessors that build one live on
/// [`Library`](crate::app::library::Library) rather than on `App`.
#[derive(Clone, Copy)]
pub(crate) struct GridLayout<'a> {
    groups: &'a [Group],
    columns: usize,
}

impl<'a> GridLayout<'a> {
    pub(crate) fn new(groups: &'a [Group], columns: usize) -> Self {
        Self {
            groups,
            columns: columns.max(1),
        }
    }

    pub(crate) fn columns(&self) -> usize {
        self.columns
    }

    /// Every group with its geometry, in grid order. One pass of integer adds over at most
    /// [`MAX_GROUPS`] entries and no allocation — the whole reason the derived fields are not
    /// stored: nothing can go stale when the column count changes.
    pub(crate) fn placed(&self) -> impl Iterator<Item = Placed<'a>> + '_ {
        let columns = self.columns;
        let mut first_row = 0;
        let mut y_offset = 0;
        self.groups.iter().enumerate().map(move |(i, group)| {
            let rows = group.len.div_ceil(columns);
            // Each heading takes one line; all but first add gap (visual block separation).
            y_offset += SECTION_HEADING_H + if i == 0 { 0 } else { SECTION_GAP };
            let placed = Placed {
                group,
                first_idx: first_row * columns,
                first_row,
                rows,
                y_offset,
            };
            first_row += rows;
            placed
        })
    }

    /// The group holding grid slot `idx`, skipping the padding after a partial last row.
    fn at_idx(&self, idx: usize) -> Option<Placed<'a>> {
        self.placed().find(|p| p.slots().contains(&idx))
    }

    /// Grid slots in total, padding between groups included but none after the last.
    pub(crate) fn len(&self) -> usize {
        self.placed().last().map_or(0, |p| p.slots().end)
    }

    pub(crate) fn rows(&self) -> usize {
        self.len().div_ceil(self.columns)
    }

    /// Section headings to draw: each group's name and the slot it sits above. Empty groups
    /// never reach here — [`Library::regroup`](crate::app::library::Library::regroup) drops
    /// them, since a zero-row section has nothing to hang a heading on.
    pub(crate) fn headings(&self) -> impl Iterator<Item = (usize, &'a Group)> + '_ {
        self.placed().map(|p| (p.first_idx, p.group))
    }

    /// The card at grid index `idx` — `None` for the padding after a group's partial last row.
    pub(crate) fn card_at(&self, games: &'a [GameEntry], idx: usize) -> Option<&'a GameEntry> {
        let placed = self.at_idx(idx)?;
        games.get(placed.group.games_start + (idx - placed.first_idx))
    }

    /// The pin id for whatever's at grid index `idx` — a `GameEntry::id`, `None` for the
    /// padding after a partial last row.
    pub(crate) fn pin_id_at(&self, games: &'a [GameEntry], idx: usize) -> Option<&'a str> {
        Some(self.card_at(games, idx)?.id.as_str())
    }

    pub(crate) fn idx_for_pin_id(&self, games: &[GameEntry], id: &str) -> Option<usize> {
        let pos = games.iter().position(|g| g.id == id)?;
        let placed = self
            .placed()
            .find(|p| (p.group.games_start..p.group.games_start + p.group.len).contains(&pos))?;
        Some(placed.first_idx + (pos - placed.group.games_start))
    }

    /// The grid's rows split into the bands that share a vertical offset — one per group, each
    /// uniformly spaced inside itself, so a band's visible rows are a closed-form range.
    pub(crate) fn row_bands(&self) -> impl Iterator<Item = (Range<usize>, i32)> + '_ {
        self.placed().map(|p| (p.rows_range(), p.y_offset))
    }

    /// Extra vertical offset carried by grid row `row`: whatever headings and gaps stack above
    /// it. Non-decreasing in `row`, which is what makes a card's y monotone in its index — see
    /// [`visible_cards`](crate::app::view::home::visible_cards).
    pub(crate) fn row_offset_at(&self, row: usize) -> i32 {
        self.placed()
            .take_while(|p| p.first_row <= row)
            .last()
            .map_or(0, |p| p.y_offset)
    }

    /// [`Self::row_offset_at`] for the row grid index `idx` sits in.
    pub(crate) fn row_offset(&self, idx: usize) -> i32 {
        self.row_offset_at(idx / self.columns)
    }

    /// What the sections add to the grid's total height — the offset its last row carries.
    pub(crate) fn total_extra(&self) -> i32 {
        self.placed().last().map_or(0, |p| p.y_offset)
    }
}

/// What `App` keeps for the grid between frames.
///
/// The dirty flags are the contract between the two halves: the event side sets them when it
/// changes something the render side cannot see, and `prepare_grid` clears them as it acts.
pub(crate) struct GridState {
    /// Every resident card by id, with its arrival clock. Keyed by identity rather than by
    /// grid position because pinning a game reorders the grid.
    pub arrivals: Arrivals,
    /// The keep window the last eviction pass ran for; an unchanged one is skipped.
    pub kept: Range<usize>,
    /// When the last-armed card pop finishes (one comparison instead of walking every card).
    /// Never lowered: late deadline costs one frame; early would freeze zoom mid-way.
    pub card_pop_until: Option<Instant>,
    /// Screen-derived card size, not rasterized.
    pub card_size: (u32, u32),
    /// All card tiles stale (games/host changed); also stands the reveal spinner up.
    pub dirty: bool,
    /// Individual card tiles stale (cover art), by pin id (cheaper than full dirty).
    pub cards_dirty: Vec<String>,
    /// Scroll offset rendered this frame; eases toward `scroll_target`.
    pub scroll: i32,
    pub scroll_target: i32,
    /// Grid re-entry from sidebar. Cleared by focus map when no longer valid.
    pub focus_last: usize,
    /// Spinner state until initial build finishes.
    pub reveal: GridReveal,
}

/// The cards resident in the build window and when each arrived. A card is noted the first
/// time it enters the window; on a settled grid that first sighting is what pops.
#[derive(Default)]
pub(crate) struct Arrivals {
    seen: std::collections::HashMap<String, Option<Entrance>>,
}

impl Arrivals {
    /// Notes `id` as resident; true the first time.
    pub fn note(&mut self, id: &str) -> bool {
        if self.seen.contains_key(id) {
            return false;
        }
        self.seen.insert(id.to_string(), None);
        true
    }

    pub fn pop(&self, id: &str) -> Option<Entrance> {
        self.seen.get(id).copied().flatten()
    }

    /// Starts `id`'s arrival; false when it is not resident.
    pub fn arm(&mut self, id: &str, entrance: Entrance) -> bool {
        match self.seen.get_mut(id) {
            Some(slot) => {
                *slot = Some(entrance);
                true
            }
            None => false,
        }
    }

    pub fn release(&mut self, id: &str) {
        self.seen.remove(id);
    }

    pub fn clear(&mut self) {
        self.seen.clear();
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.seen.keys().map(String::as_str)
    }
}

/// Card arrival: scale-up on the grid. Not first appearance (that is `GridReveal`'s wave).
#[derive(Clone, Copy)]
pub(crate) struct Entrance {
    pub start: Instant,
}

impl Entrance {
    pub fn pop(start: Instant) -> Self {
        Self { start }
    }

    /// Arrival progress and pop shrink, on the caller's clock.
    pub fn progress(self, now: Instant) -> (f32, f32) {
        let frac = crate::ui::animation::anim_frac_at(Some(self.start), crate::app::CARD_POP, now);
        (frac, crate::app::CARD_POP_SHRINK)
    }

    /// [`Self::progress`] for a card that may not be arriving: `(1.0, 0.0)` when it is not.
    pub fn progress_of(entrance: Option<Self>, now: Instant) -> (f32, f32) {
        entrance.map_or((1.0, 0.0), |e| e.progress(now))
    }

    /// When the arrival finishes (redraw loop deadline).
    pub fn end(self) -> Instant {
        self.start + crate::app::CARD_POP
    }
}

impl GridState {
    /// Starts/restarts `pin_id`'s pop. Cards without tiles get their clock on build.
    /// Grid's first appearance uses `GridReveal` mask instead (one surface, not per-card entrance).
    pub fn arm_card_pop(&mut self, pin_id: &str, at: Instant) {
        let entrance = Entrance::pop(at);
        if self.arrivals.arm(pin_id, entrance) {
            self.extend_pop_deadline(entrance.end());
        }
    }

    /// Raises the redraw deadline to `end`. Never lowered: a deadline that is merely late
    /// costs one extra frame of the redraw loop, where an early one would freeze an arrival
    /// mid-way.
    fn extend_pop_deadline(&mut self, end: Instant) {
        if self.card_pop_until.is_none_or(|until| until < end) {
            self.card_pop_until = Some(end);
        }
    }

    /// Whether any card is still arriving.
    pub fn card_pops_running(&self) -> bool {
        self.card_pop_until.is_some_and(|until| Instant::now() < until)
    }
}

/// Hand-written rather than derived, because two of these fields do not want their type's zero
/// value: the first library load is what fills the grid and has to read as stale to be built at
/// all, and nothing is waiting until a host is picked.
impl Default for GridState {
    fn default() -> Self {
        Self {
            arrivals: Arrivals::default(),
            kept: 0..0,
            card_pop_until: None,
            card_size: (0, 0),
            dirty: true,
            cards_dirty: Vec::new(),
            scroll: 0,
            scroll_target: 0,
            focus_last: 0,
            reveal: GridReveal::revealed(),
        }
    }
}
