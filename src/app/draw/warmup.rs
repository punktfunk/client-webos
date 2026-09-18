//! Exercise entrance and settled rendering programs before the first interaction.

use pf_console_ui::theme::{self, Fonts};
use pf_console_ui::widgets::RowSpec;
use skia_safe::{Rect, Surface};

use super::{glass, home, list, settings, FocusEase, Frame};
use crate::app::render::state::ListSlot;
use crate::app::state::settingspage::Page;
use crate::core::screen::Screen;

/// Warmup samples, and how many of them the arrival ramp spans — the rest hold the settled
/// look, which compiles the programs a still menu draws.
const STEPS: usize = 64;
const ARRIVAL: f32 = 12.0;

pub(crate) fn draw(
    surface: &mut Surface,
    fonts: &Fonts,
    w: u32,
    h: u32,
    mut submit: impl FnMut(&mut Surface),
) -> Option<()> {
    let info = surface.image_info();
    let mut target = surface.new_surface(&info)?;
    target.canvas().clear(theme::card_face(0.0));
    target.canvas().draw_rect(
        Rect::from_xywh(0.0, 0.0, info.width() as f32 / 2.0, info.height() as f32),
        &theme::fill(theme::fg(0.3)),
    );
    let source = target.image_snapshot();
    let k = super::scale(h);
    // The frame's own drawable-over-layout scale, shared by the blur sigma and the canvas.
    let (sx, sy) = (
        info.width() as f32 / w.max(1) as f32,
        info.height() as f32 / h.max(1) as f32,
    );
    let page = glass::Page::capture(target.canvas(), &source, glass::page_sigma(h, info.height() as u32))?;
    // The same format the library decodes covers into: an unpremultiplied RGBA8 raster. A
    // premultiplied stand-in compiled different sampling programs, so the first real card
    // menu compiled its own while it slid up.
    let (cw, ch) = (480u32, 720u32);
    let pixels: Vec<u8> = (0..cw * ch)
        .flat_map(|i| {
            if (i % cw).abs_diff(cw / 2) < cw / 4 {
                [200, 180, 160, 255]
            } else {
                [20, 24, 28, 255]
            }
        })
        .collect();
    let cover = home::raw_image(cw, ch, home::RawFormat::Rgba8888, &pixels)?;
    let rows = [RowSpec::action("Connect", true), RowSpec::action("Settings", true)];
    let settings_rows = [
        RowSpec::choice("Resolution", "Native").with_header("Display"),
        RowSpec::slider("Bitrate", "Automatic", 0.0),
        RowSpec::toggle("HDR", true),
        RowSpec::toggle("Statistics", false),
        RowSpec::choice("Palette", "Violet").with_header("Interface"),
        RowSpec::toggle("Gamepad UI", false),
        RowSpec::choice("Mode", "Automatic"),
        RowSpec::action("About", true).with_header("Information"),
        RowSpec::action("Licences", true),
        RowSpec::choice("Codec", "Automatic").with_header("Video"),
        RowSpec::toggle("10-bit SDR", false),
        RowSpec::toggle("Smooth buffer", true),
        RowSpec::toggle("Game mode", false),
    ];
    let mut host_menu = ListSlot::new(Screen::HostMenu);
    let mut settings_menu = ListSlot::new(Screen::SettingsPage);
    let mut tabs = FocusEase::default();
    let l = list::layout(fonts, w as f32, h as f32, k, Some("Host"), rows.len(), 0);
    let s = settings::layout(w as f32, h as f32, k);
    // Step widget clocks through arrival and settling, without sleeping at startup.
    for step in 0..STEPS {
        let progress = (step as f32 / ARRIVAL).min(1.0);
        // Overflow adds a masked layer absent from short menus. Exercise both edges.
        if step == STEPS / 2 {
            settings_menu.list.cursor = settings_rows.len() - 1;
        }
        {
            let c = target.canvas();
            c.reset_matrix();
            c.clear(theme::card_face(0.0));
            c.scale((sx, sy));
            fonts.begin_frame();
            let f = Frame::new(c, fonts, w, h).with_backdrop(Some(page.backdrop()));
            let dy = crate::ui::animation::modal_rise(progress) as f32;
            list::draw(
                &f,
                &mut host_menu,
                &l,
                "Host",
                &rows,
                false,
                progress,
                dy,
                1.0 / 60.0,
                true,
            );
            settings::draw(
                &f,
                &mut settings_menu,
                &mut tabs,
                &s,
                Page::General,
                true,
                &settings_rows,
                false,
                progress,
                dy,
                1.0 / 60.0,
                true,
            );
            home::warm_card_strip(&f, &cover, progress);
            if step + 1 == STEPS {
                glass::glass_card(&Frame::new(c, fonts, w, h), l.card, 16.0);
            }
        }
        // Submit every sample: later clears must not discard earlier warmup draws.
        submit(&mut target);
    }
    glass::clear_cover();
    Some(())
}
