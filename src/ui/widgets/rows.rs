//! `FocusRow`/`RowKind`: what a list row says. Drawn by the console kit's list through
//! `app::draw::list::row_spec`; this is the app-side description the screens build.
use crate::ui::focus::Dir;

/// How a focus row's right-hand control behaves — every row list in the app shares
/// [`FocusRows`]' single implementation, see its docs.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum RowKind {
    Dropdown,
    Toggle,
    /// A plain actionable row — icon + label, with `value` (if any) as a muted hint on
    /// the right and no control at all. This is what makes a screen out of nothing but
    /// a list: see [`ListModal`], which the host-actions menu and
    /// the Settings sub-page links are both built from. Confirm on the row *is* the
    /// action; there is nothing to adjust in place.
    Action,
}

/// One focusable icon + label (+ dropdown pill / slider / switch) row, drawn by
/// [`FocusRows`]/[`Canvas::focus_row`]. The lists themselves are built per screen in
/// `app::view`.
#[derive(Clone)]
pub struct FocusRow {
    pub icon: &'static str,
    pub label: String,
    pub value: String,
    pub kind: RowKind,
    /// Destructive action (Forget host) — drawn in `palette().error` rather than the
    /// normal muted/white pair, so it reads as dangerous before it's confirmed.
    pub danger: bool,
    /// The row is shown but its value cannot be changed here — dictated by another setting or
    /// by the hardware (e.g. HDR under an H.264 codec pick). Only the *control* greys out
    /// (`palette().disabled`); icon and label keep their normal focus colors, so the row still
    /// reads as a live list entry whose value happens to be fixed. Still focusable, so its
    /// [`subtext`](Self::subtext) — where the reason belongs — can be read. Rejecting the
    /// input is the caller's business, not this widget's.
    pub locked: bool,
    /// Icon buttons at the row's right end, in left-to-right order — drawn and focused
    /// exactly like a sidebar host row's ⋯ ([`Canvas::sidebar_menu_button`]). Each is a
    /// further thing Confirm can mean on this row, reached with Right, so the row's own
    /// action stays a single press. Per-row, so one row in a list can offer fewer than its
    /// neighbours (Library has no Remove). Empty (the default) draws nothing.
    pub trailing: &'static [&'static str],
    /// Room to keep clear at the row's right end, in trailing buttons, when placing the
    /// value label — over and above the buttons this row actually has. A list whose rows
    /// carry different counts (Library has no Remove) would otherwise hang its values at
    /// different x; [`align_values`] sets this so the column lines up. 0 (the default)
    /// reserves exactly the row's own buttons.
    pub value_reserve: usize,
    /// The row's own [`icon`](Self::icon) is a button too, in the slot it already draws in —
    /// reached with Left, where [`trailing`](Self::trailing) is reached with Right. For the
    /// action a row wants *before* its label rather than after it (a collection's drag
    /// handle), so the grip sits where the eye starts the row.
    pub leading_button: bool,
    /// A dot in the row's gutter: the row differs from what it inherits.
    pub marked: bool,
    /// Keep the [`mark`](Self::mark) gutter clear even though this row wears no dot — what
    /// [`align_values`] sets on a list where some *other* row does, so one marked row does
    /// not pull its own value 32px left of its neighbours'.
    pub mark_reserve: bool,
    /// A small secondary line drawn under the row's label *only while the row is focused*
    /// (e.g. the high-bitrate "may be unstable on Wi-Fi" caution on the Bitrate row).
    /// Unfocused, the label stays vertically centred and nothing is drawn; on focus the
    /// label + caption centre as a block. `None` never draws anything.
    pub subtext: Option<RowSubtext>,
}

/// A small caption drawn under a row's label. The color is carried with the text so the
/// drawing code stays generic — a neutral hint and a caution differ only by which
/// constructor built them, not by any special-casing in [`Canvas::focus_row`].
#[derive(Clone)]
pub struct RowSubtext {
    pub text: String,
}

impl RowSubtext {
    /// Neutral secondary line (muted grey) — the default for extra context (e.g. the
    /// rooted-TV note on the experimental Game mode row).
    pub fn hint(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }

    /// Dull-orange caution line — a soft warning that isn't a hard error.
    pub fn caution(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

impl FocusRow {
    /// A plain [`RowKind::Action`] row — the common case for list-modal screens.
    pub fn action(icon: &'static str, label: impl Into<String>) -> Self {
        Self {
            icon,
            label: label.into(),
            value: String::new(),
            kind: RowKind::Action,
            danger: false,
            locked: false,
            trailing: &[],
            value_reserve: 0,
            leading_button: false,
            marked: false,
            mark_reserve: false,
            subtext: None,
        }
    }

    /// Same, with a muted right-hand hint (e.g. a host's address under its name).
    pub fn action_with_value(icon: &'static str, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            ..Self::action(icon, label)
        }
    }

    /// A [`RowKind::Toggle`] row; `on` picks the On/Off value the switch animates from
    /// ([`Canvas::focus_row`] reads it back out of `value`).
    pub fn toggle(icon: &'static str, label: impl Into<String>, on: bool) -> Self {
        Self {
            kind: RowKind::Toggle,
            value: if on { "On".into() } else { "Off".into() },
            ..Self::action(icon, label)
        }
    }

    /// A [`RowKind::Dropdown`] row showing `value` as its current pick.
    pub fn dropdown(icon: &'static str, label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            kind: RowKind::Dropdown,
            ..Self::action_with_value(icon, label, value)
        }
    }

    /// Marks this row destructive (see [`FocusRow::danger`]).
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Greys this row out and marks its value fixed (see [`FocusRow::locked`]).
    pub fn locked(mut self, locked: bool) -> Self {
        self.locked = locked;
        self
    }

    /// Adds a muted caption line under the label.
    pub fn with_subtext(mut self, subtext: RowSubtext) -> Self {
        self.subtext = Some(subtext);
        self
    }

    /// Puts a [`FocusRow::mark`] dot in this row's right gutter.
    pub fn marked(mut self) -> Self {
        self.marked = true;
        self
    }

    /// Gives this row [`trailing`](Self::trailing) buttons. Always built unfocused: which
    /// one is lit belongs to the focused-row tile alone (see `App::list_focus_rows`), so a
    /// shell underneath cannot bake in a highlight that outlives the focus that put it there.
    pub fn with_trailing(mut self, icons: &'static [&'static str]) -> Self {
        self.trailing = icons;
        self
    }

    /// Makes this row's icon a [`leading_button`](Self::leading_button). Always built
    /// unfocused, for the same reason [`with_trailing`](Self::with_trailing) is.
    pub fn with_leading_button(mut self) -> Self {
        self.leading_button = true;
        self
    }
}

/// Puts every row's value label in one column: each reserves the widest row's
/// trailing-button room, and the mark gutter if any row is marked
/// ([`FocusRow::value_reserve`], [`FocusRow::mark_reserve`]). A right-aligned value otherwise
/// stops wherever its own row's buttons and dot leave off, so one odd row out looks ragged.
pub fn align_values(rows: &mut [FocusRow]) {
    let widest = rows.iter().map(|r| r.trailing.len()).max().unwrap_or(0);
    let any_marked = rows.iter().any(|r| r.marked);
    for row in rows {
        row.value_reserve = widest;
        row.mark_reserve = any_marked;
    }
}

/// Navigate within a list, wrapping. Returns true if focus moved.
pub fn list_nav(focused: &mut usize, len: usize, dir: Option<Dir>) -> bool {
    if len == 0 {
        return false;
    }
    match dir {
        Some(Dir::Up) => {
            *focused = if *focused == 0 { len - 1 } else { *focused - 1 };
            true
        }
        Some(Dir::Down) => {
            *focused = (*focused + 1) % len;
            true
        }
        _ => false,
    }
}
