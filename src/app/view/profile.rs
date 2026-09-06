//! The profile editor's copy: the rename form and the delete confirmation.

pub const RENAME_TITLE: &str = "Name this profile";
pub const RENAME_SUBTITLE: &str =
    "What the profile is called wherever it is listed — in the host menu, on a card, and on every other client.";
pub const DELETE_TITLE: &str = "Delete profile?";

/// What falls back to the default settings when the profile goes.
pub fn delete_subtitle(hosts: usize, titles: usize) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    match (hosts, titles) {
        (0, 0) => "Nothing uses it yet. The profile and its overrides are discarded.".to_string(),
        (h, 0) => format!("{} will go back to the default settings.", plural(h, "host", "hosts")),
        (0, t) => format!("{} will go back to the default settings.", plural(t, "title", "titles")),
        (h, t) => format!(
            "{} and {} will go back to the default settings.",
            plural(h, "host", "hosts"),
            plural(t, "title", "titles")
        ),
    }
}
