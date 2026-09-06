//! The "add this card to…" modal — presentation: its rows and its shell. Logic lives in
//! `app::state::collections`.
//!
//! A scrolling row list rather than a plain one: a host may hold every collection
//! `MAX_COLLECTIONS` allows plus the dynamic Library entry, which is a card taller than the
//! screen if baked into a single tile (see `view::scrolllist`).
use crate::app::view::icons;
use crate::core::model::KnownHost;
use crate::ui;
use crate::ui::widgets::FocusRow;

const TITLE: &str = "Add to";
const MOVE_TITLE: &str = "Move to";
/// What the list is doing, by whether a collection already holds the card: one that sits in
/// Library can only be gained by a collection, one that is held leaves its own behind. The
/// card menu's row ([`menu_row_label`]) opens this modal, so both read it from here and the
/// two can't drift apart.
pub(crate) fn heading(held: bool) -> &'static str {
    if held {
        MOVE_TITLE
    } else {
        TITLE
    }
}

/// [`heading`] as the card menu's row reads it — the same words, with the ellipsis that says
/// it opens something.
pub(crate) fn menu_row_label(held: bool) -> &'static str {
    if held {
        "Move to\u{2026}"
    } else {
        "Add to\u{2026}"
    }
}

pub(crate) const ADD_ROW: &str = "Add collection";
pub(crate) const REMOVE_TITLE: &str = "Remove collection?";
/// Replaces the card's name in the heading while a row is being dragged: the list is doing
/// something else for the moment, and the only slot that says so is already drawn per frame.
pub(crate) const DRAG_HINT: &str = "Up/Down to move, OK to drop";
pub(crate) const ADD_TITLE: &str = "New collection";
pub(crate) const RENAME_TITLE: &str = "Rename collection";

/// The name dialog's subtitle. Adding says where the card is about to land, because the add
/// row moves it in one go rather than dropping the user back on the list to pick what they
/// just named.
pub(crate) fn name_subtitle(renaming: Option<&str>, card: &str) -> String {
    match renaming {
        Some(old) => format!("A new name for {old}."),
        None => format!("Name it, and {card} moves into it."),
    }
}

/// What a collection row's trailing buttons are — the reorder grip is not among them: it is
/// the row's leading button, in the icon slot (see [`rows`]). Library keeps Rename but has no
/// Remove at all — `KnownHost::remove_collection` refuses it too, so the missing icon is the affordance
/// rather than the rule.
pub(crate) fn trailing(dynamic: bool) -> &'static [&'static str] {
    if dynamic {
        &[icons::ICON_EDIT]
    } else {
        &[icons::ICON_EDIT, icons::ICON_DELETE]
    }
}

/// The remove dialog's subtitle: what happens to the cards it holds, which is the whole
/// question — nothing is deleted, the games come back to Library.
/// [`trailing`] in the kit's marks: what the row's buttons draw.
pub(crate) fn trailing_marks(dynamic: bool) -> &'static [&'static str] {
    if dynamic {
        &["pencil"]
    } else {
        &["pencil", "trash-2"]
    }
}

pub(crate) fn remove_subtitle(name: &str, games: usize) -> String {
    let games = match games {
        0 => "It holds no games".to_string(),
        1 => "Its 1 game returns to Library".to_string(),
        n => format!("Its {n} games return to Library"),
    };
    format!("{name} will be removed. {games}.")
}

/// Why the typed name cannot be committed — `None` while it can. Blank reads as unfinished
/// rather than as an error, so only a name that is taken says anything.
pub(crate) fn name_hint(host: &KnownHost, at: Option<usize>, typed: &str) -> Option<&'static str> {
    let typed = typed.trim();
    (!typed.is_empty() && !host.can_name(at, typed)).then_some("Already used")
}

/// One row per entry in grid order, Library included. `holding` is the collection the card
/// being moved is in right now (`None` for Library, its implicit home) — that row wears the
/// mark dot, so the list opens saying where the card already is instead of leaving the user
/// to work it out.
pub(crate) fn rows(host: &KnownHost, holding: Option<usize>) -> Vec<FocusRow> {
    let library = host.library_index();
    let mut rows: Vec<FocusRow> = host
        .collections()
        .iter()
        .enumerate()
        .map(|(i, collection)| {
            let count = if collection.dynamic {
                // Library's members are whatever no one else claims, so its count is not in
                // the vector — and saying "0 games" of it would be a lie.
                None
            } else {
                Some(collection.games.len())
            };
            // The grip stands in for the folder pictogram rather than sitting beside it: a
            // row this wide with an icon at each end reads as two controls and a label
            // between them, and the drag handle is the one worth pointing at.
            let row = FocusRow::action_with_value(icons::ICON_REORDER, collection.name.clone(), count_label(count))
                .with_trailing(trailing(collection.dynamic))
                .with_leading_button();
            let row = match count {
                // An empty collection is hidden in the grid, which reads as a vanished one
                // unless the row that still lists it says so.
                Some(0) => row.with_subtext(ui::widgets::RowSubtext::hint("Hidden until you add a game")),
                _ => row,
            };
            if holding == Some(i) || (holding.is_none() && library == Some(i)) {
                row.marked()
            } else {
                row
            }
        })
        .collect();
    // Last, and only while there is room: a row that would refuse the dialog it opens is
    // worse than no row (`MAX_COLLECTIONS`).
    if host.can_add_collection() {
        rows.push(FocusRow::action(icons::ICON_ADD, ADD_ROW.to_string()));
    }
    // Library has no Remove and one row wears the mark dot, both of which would otherwise
    // shift that row's count.
    ui::widgets::align_values(&mut rows);
    rows
}

/// How many rows [`rows`] builds — the count without their labels, which the compose,
/// hit-test and scroll paths ask for per frame.
pub(crate) fn row_count(host: &KnownHost) -> usize {
    host.collections().len() + usize::from(host.can_add_collection())
}

fn count_label(count: Option<usize>) -> String {
    match count {
        Some(1) => "1 game".to_string(),
        // "0 games" is noise next to the subtext that already says the row is hidden, and
        // Library has no count at all.
        Some(0) | None => String::new(),
        Some(n) => format!("{n} games"),
    }
}
