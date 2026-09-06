//! The selected host's library: its games, their art, and the grid grouping read every frame.
//!
//! Grouped so the grid's `&self` geometry queries can borrow the library disjointly from the
//! `&mut self` paths that mutate it. Everything here is per-host state and is replaced wholesale
//! on a host switch (`App::clear_selected_host`).

use std::cmp::Reverse;
use std::collections::HashMap;

use crate::services::art::CardArt;

use crate::app::grid::{GridLayout, Group};
use crate::core::model::{GameEntry, KnownHost, DESKTOP_PIN_ID};
use crate::services::recents::HostRecents;

#[derive(Default)]
pub(crate) struct Library {
    pub(crate) selected_host: Option<(String, u16)>,
    /// Every game, laid out group by group: each [`Group`]'s games are one contiguous run
    /// starting at its `games_start`. [`Self::regroup`] is what maintains that.
    pub(crate) games: Vec<GameEntry>,
    /// The grid's sections, in grid order. Empty collections are dropped here rather than
    /// rendered as a heading over nothing; the collections modal still lists them.
    ///
    /// Kept rather than derived on demand because `layout` is asked for a card rect on every
    /// frame and every pointer motion, and deriving it meant scanning `known_hosts` per call.
    /// Every path that changes a collection, the selected host or the library goes through
    /// [`Self::regroup`] (or drops the grid via [`Self::clear_groups`]).
    pub(crate) groups: Vec<Group>,
    /// Cover art pixmaps by game id.
    pub(crate) art: HashMap<String, CardArt>,
}

/// The Desktop card, as an ordinary library entry. It is a card the *client* offers rather
/// than one the host lists, so nothing else about it is special: it is named, collected,
/// ordered and moved exactly like a game (see [`Library::load_games`]).
fn desktop_entry() -> GameEntry {
    GameEntry {
        id: DESKTOP_PIN_ID.to_string(),
        title: "Desktop".to_string(),
        art: crate::core::model::Artwork::default(),
        icon: None,
    }
}

impl Library {
    /// Takes the host's listing as this library's games, Desktop among them, alphabetically.
    ///
    /// The host returns its own scan order, which is neither stable nor meaningful to a
    /// reader. On a TV the grid is navigated a card at a time with a d-pad, so alphabetical is
    /// the difference between "find the game" and "sweep the whole library". Case-insensitive
    /// so casing doesn't scatter otherwise-adjacent titles. A collection then imposes its own
    /// order over its members, and the dynamic Library entry re-sorts by recency in
    /// [`Self::regroup`] — this is what both of those start from.
    pub(crate) fn load_games(&mut self, mut games: Vec<GameEntry>) {
        games.push(desktop_entry());
        games.sort_by_key(|g| g.title.to_lowercase());
        self.games = games;
    }

    /// Desktop's icon is client-picked (every other entry carries the host's token), so it is
    /// chosen here rather than in the grid build loop. Idempotent: re-run on every regroup and
    /// whenever mDNS teaches a new OS.
    pub(crate) fn set_desktop_icon(&mut self, os: &str) {
        let token = crate::app::assets::os_icon_token(os);
        if let Some(desktop) = self.games.iter_mut().find(|g| g.id == DESKTOP_PIN_ID) {
            desktop.icon = token;
        }
    }

    /// The grid's shape at `columns` columns. `Copy` and borrowing only `groups`, so a caller
    /// can hold one and go on mutating the rest of `App` — the disjointness this module exists
    /// for.
    pub(crate) fn layout(&self, columns: usize) -> GridLayout<'_> {
        GridLayout::new(&self.groups, columns)
    }

    pub(crate) fn grid_len(&self, columns: usize) -> usize {
        self.layout(columns).len()
    }

    pub(crate) fn card_at(&self, idx: usize, columns: usize) -> Option<&GameEntry> {
        self.layout(columns).card_at(&self.games, idx)
    }

    pub(crate) fn pin_id_at(&self, idx: usize, columns: usize) -> Option<&str> {
        self.layout(columns).pin_id_at(&self.games, idx)
    }

    pub(crate) fn idx_for_pin_id(&self, id: &str, columns: usize) -> Option<usize> {
        self.layout(columns).idx_for_pin_id(&self.games, id)
    }

    /// Re-lays the library out over `host`'s collections: each collection's members in its own
    /// order, the dynamic Library entry taking whatever is left — sorted by `recents`, wherever
    /// in the order it sits.
    /// One pass over the games, so it costs the same whether one card moved or the whole
    /// library arrived.
    ///
    /// A member the host no longer lists just doesn't place — it is *not* dropped here, since
    /// this also runs while `games` is empty (an offline host). Dropping is
    /// `KnownHost::prune_games`' job.
    pub(crate) fn regroup(&mut self, host: &KnownHost, recents: &HostRecents) {
        let collections = host.collections();
        if collections.is_empty() {
            self.clear_groups();
            return;
        }
        // Taken by value so a game can only be placed once; whatever survives is Library's.
        let mut remaining: Vec<Option<GameEntry>> = std::mem::take(&mut self.games).into_iter().map(Some).collect();
        let by_id: HashMap<&str, usize> = remaining
            .iter()
            .enumerate()
            .filter_map(|(i, g)| g.as_ref().map(|g| (g.id.as_str(), i)))
            .collect();

        // Where each collection's games land, resolved before anything is moved out of
        // `remaining` — `by_id` borrows it, and the placement below consumes it.
        let placement: Vec<Vec<usize>> = collections
            .iter()
            .map(|c| {
                c.games
                    .iter()
                    .filter_map(|id| by_id.get(id.as_str()).copied())
                    .collect()
            })
            .collect();
        // Library takes what no collection claims — and a collection ordered *after* it is
        // still a claim. Without this, a card moved into a freshly added collection (which
        // `add_collection` pushes last) was swallowed by the dynamic run before its own
        // group was reached, and the group placed empty and was dropped.
        let mut claimed = vec![false; remaining.len()];
        for at in placement.iter().flatten() {
            claimed[*at] = true;
        }

        let mut games = Vec::with_capacity(remaining.len());
        let mut groups = Vec::with_capacity(collections.len());
        for (i, collection) in collections.iter().enumerate() {
            let games_start = games.len();
            if collection.dynamic {
                games.extend(
                    remaining
                        .iter_mut()
                        .zip(&claimed)
                        .filter(|(_, claimed)| !**claimed)
                        .filter_map(|(g, _)| g.take()),
                );
                // Library is ordered by play, not by the host's listing: most recent first,
                // the never-played after them. `sort_by_key` is stable, so those keep the
                // order the host gave — and a zero key sorts them last under `Reverse`.
                games[games_start..].sort_by_key(|g| Reverse(recents.get(&g.id).copied().unwrap_or(0)));
            } else {
                games.extend(placement[i].iter().filter_map(|&at| remaining[at].take()));
            }
            let len = games.len() - games_start;
            if len == 0 {
                continue;
            }
            groups.push(Group {
                name: collection.name.clone(),
                len,
                games_start,
            });
        }
        self.games = games;
        self.groups = groups;
    }

    /// Exchanges two games in place, keeping the grid's order in step with the collection's
    /// without a regroup. Both are members of one collection, so they sit in one group's run
    /// and no group's bounds move — the cards trade slots and everything else keeps its own.
    /// Tiles are keyed by pin id, so each card carries its pixels across with it.
    pub(crate) fn swap_games(&mut self, a: &str, b: &str) {
        let (Some(i), Some(j)) = (self.position(a), self.position(b)) else {
            return;
        };
        self.games.swap(i, j);
    }

    fn position(&self, id: &str) -> Option<usize> {
        self.games.iter().position(|g| g.id == id)
    }

    /// Forgets the grouping the grid is drawn from — for the paths that drop the library
    /// itself, where there is no host left to recompute it from.
    pub(crate) fn clear_groups(&mut self) {
        self.groups.clear();
    }

    /// Drops everything drawn from the current host's library. The grid, the grouping and the
    /// art go together: a group indexes into `games`, so leaving one behind draws the previous
    /// host's cards.
    pub(crate) fn clear(&mut self) {
        self.games = Vec::new();
        self.clear_groups();
        self.art.clear();
    }
}
