//! What the app resolves for its own marks. The fonts, the icon marks and the launcher
//! marks all come from the console kit now (`pf_console_ui`); the one app-owned asset left is
//! the app icon `app::draw::home` embeds for the sidebar.

/// The card icon token for an advertised OS chain — `os/linux/fedora/bazzite`, kept whole:
/// the kit's resolver walks it most-specific-first when the mark is drawn. `None` when no
/// token in the chain has a mark, so the Desktop card falls back to its title.
pub fn os_icon_token(chain: &str) -> Option<String> {
    let probe = skia_safe::Rect::from_wh(16.0, 16.0);
    pf_console_ui::os_marks::os_mark(chain, probe)
        .is_some()
        .then(|| format!("os/{chain}"))
}
