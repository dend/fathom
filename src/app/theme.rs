//! Colours, fonts and spacing. Styled after native desktop file managers:
//! dense rows, flat chrome, near-square corners and system fonts.

use std::sync::Arc;

use eframe::egui::{
    self, style::ScrollStyle, style::WidgetVisuals, Color32, CornerRadius, FontData,
    FontDefinitions, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle, Theme, Visuals,
};

#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub bg: Color32,
    pub chrome: Color32,
    pub raised: Color32,
    pub inset: Color32,
    pub border: Color32,
    pub border_strong: Color32,
    pub text: Color32,
    pub muted: Color32,
    pub faint: Color32,
    pub accent: Color32,
    pub accent_text: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub selected: Color32,
    pub selected_inactive: Color32,
    pub danger: Color32,
    pub success: Color32,
    pub folder: Color32,
    pub bucket: Color32,
    pub image: Color32,
    pub video: Color32,
    pub audio: Color32,
    pub code: Color32,
    pub text_file: Color32,
    pub archive: Color32,
    pub file: Color32,
}

const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub const LIGHT: Palette = Palette {
    bg: rgb(0xffffff),
    chrome: rgb(0xf3f3f3),
    raised: rgb(0xffffff),
    inset: rgb(0xebebeb),
    border: rgb(0xdcdcdc),
    border_strong: rgb(0xc2c2c2),
    text: rgb(0x1a1a1a),
    muted: rgb(0x5f5f5f),
    faint: rgb(0x8a8a8a),
    accent: rgb(0x0f62c9),
    accent_text: rgb(0xffffff),
    hover: Color32::from_rgba_premultiplied(0, 0, 0, 14),
    pressed: Color32::from_rgba_premultiplied(0, 0, 0, 26),
    selected: rgb(0xcfe3fb),
    selected_inactive: rgb(0xe3e3e3),
    danger: rgb(0xc42b1c),
    success: rgb(0x0f7b3f),
    folder: rgb(0xdc9c2e),
    bucket: rgb(0x0f62c9),
    image: rgb(0x8a4fd0),
    video: rgb(0xcf3f44),
    audio: rgb(0x0b8a7e),
    code: rgb(0x2b8a3e),
    text_file: rgb(0x5c6773),
    archive: rgb(0xa55f14),
    file: rgb(0x7f8791),
};

pub const DARK: Palette = Palette {
    bg: rgb(0x1e1e1e),
    chrome: rgb(0x272727),
    raised: rgb(0x2c2c2c),
    inset: rgb(0x171717),
    border: rgb(0x3a3a3a),
    border_strong: rgb(0x505050),
    text: rgb(0xe8e8e8),
    muted: rgb(0xa8a8a8),
    faint: rgb(0x7c7c7c),
    accent: rgb(0x4c9bf5),
    accent_text: rgb(0x0b1522),
    hover: Color32::from_rgba_premultiplied(18, 18, 18, 18),
    pressed: Color32::from_rgba_premultiplied(31, 31, 31, 31),
    selected: rgb(0x1f456f),
    selected_inactive: rgb(0x383838),
    danger: rgb(0xff8478),
    success: rgb(0x5fd08a),
    folder: rgb(0xe6b04f),
    bucket: rgb(0x6fb0f8),
    image: rgb(0xb58cf0),
    video: rgb(0xf07b7e),
    audio: rgb(0x45c2b4),
    code: rgb(0x68c279),
    text_file: rgb(0x9aa5b1),
    archive: rgb(0xd39a55),
    file: rgb(0x8d949c),
};

pub fn palette(ui: &egui::Ui) -> &'static Palette {
    if ui.visuals().dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

pub fn palette_of(ctx: &egui::Context) -> &'static Palette {
    if ctx.theme() == Theme::Dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// Row height shared by the tree, the file list and the transfers list.
pub const ROW: f32 = 24.0;
pub const BODY: f32 = 13.0;
pub const SMALL: f32 = 12.0;

/// A semibold face for headings and emphasis; `RichText::strong` only changes colour.
pub fn strong(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("strong".into()))
}

pub fn body() -> FontId {
    FontId::proportional(BODY)
}

pub fn small() -> FontId {
    FontId::proportional(SMALL)
}

pub fn mono(size: f32) -> FontId {
    FontId::monospace(size)
}

pub fn install(ctx: &egui::Context) {
    install_fonts(ctx);
    ctx.style_mut_of(Theme::Light, |style| configure(style, &LIGHT, false));
    ctx.style_mut_of(Theme::Dark, |style| configure(style, &DARK, true));
}

fn configure(style: &mut egui::Style, p: &Palette, dark: bool) {
    let mut v = if dark {
        Visuals::dark()
    } else {
        Visuals::light()
    };
    let radius = CornerRadius::same(3);
    let widget = |bg: Color32, stroke: Color32| WidgetVisuals {
        bg_fill: bg,
        weak_bg_fill: bg,
        bg_stroke: Stroke::new(1.0, stroke),
        corner_radius: radius,
        fg_stroke: Stroke::new(1.0, p.text),
        expansion: 0.0,
    };
    v.widgets.noninteractive = widget(p.bg, p.border);
    v.widgets.inactive = widget(p.raised, p.border_strong);
    v.widgets.hovered = widget(p.chrome, p.faint);
    v.widgets.active = widget(p.inset, p.faint);
    v.widgets.open = widget(p.chrome, p.border_strong);

    v.panel_fill = p.bg;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.bg;
    v.faint_bg_color = p.chrome;
    v.code_bg_color = p.inset;
    v.window_stroke = Stroke::new(1.0, p.border_strong);
    v.window_corner_radius = CornerRadius::same(4);
    v.menu_corner_radius = radius;
    v.window_shadow = Shadow {
        offset: [0, 6],
        blur: 22,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 150 } else { 55 }),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 14,
        spread: 0,
        color: Color32::from_black_alpha(if dark { 140 } else { 45 }),
    };
    v.selection.bg_fill = p.selected;
    v.selection.stroke = Stroke::new(1.0, p.text);
    v.hyperlink_color = p.accent;
    v.error_fg_color = p.danger;
    v.text_cursor.stroke = Stroke::new(1.5, p.text);
    v.striped = false;
    v.indent_has_left_vline = false;
    v.collapsing_header_frame = false;
    v.interact_cursor = None;
    v.image_loading_spinners = false;
    style.visuals = v;

    let s = &mut style.spacing;
    s.item_spacing = egui::vec2(6.0, 4.0);
    s.button_padding = egui::vec2(10.0, 3.0);
    s.interact_size = egui::vec2(24.0, ROW);
    s.menu_margin = Margin::same(4);
    s.window_margin = Margin::same(14);
    s.indent = 14.0;
    s.icon_width = 14.0;
    s.icon_width_inner = 8.0;
    s.icon_spacing = 6.0;
    s.menu_width = 210.0;
    s.scroll = ScrollStyle::solid();
    s.scroll.bar_width = 10.0;

    style.text_styles = [
        (TextStyle::Small, FontId::proportional(11.5)),
        (TextStyle::Body, FontId::proportional(BODY)),
        (TextStyle::Button, FontId::proportional(BODY)),
        (TextStyle::Heading, strong(15.0)),
        (TextStyle::Monospace, FontId::monospace(12.5)),
    ]
    .into();
    style.interaction.selectable_labels = false;
    style.interaction.tooltip_delay = 0.45;
    style.animation_time = 0.08;
}

/// Uses the platform's UI and monospace fonts so text looks native. egui's
/// bundled fonts stay behind them as a fallback.
fn install_fonts(ctx: &egui::Context) {
    let windows_fonts = std::env::var("WINDIR")
        .map(|dir| format!("{dir}\\Fonts"))
        .unwrap_or_else(|_| "C:\\Windows\\Fonts".into());
    let win = |name: &str| format!("{windows_fonts}\\{name}");

    let (regular, semibold, mono): (Vec<String>, Vec<String>, Vec<String>) = if cfg!(windows) {
        (
            vec![win("segoeui.ttf")],
            vec![win("seguisb.ttf"), win("segoeuib.ttf")],
            vec![win("CascadiaMono.ttf"), win("consola.ttf")],
        )
    } else if cfg!(target_os = "macos") {
        (
            vec![
                "/System/Library/Fonts/SFNS.ttf".into(),
                "/System/Library/Fonts/HelveticaNeue.ttc".into(),
            ],
            vec![
                "/System/Library/Fonts/SFNS.ttf".into(),
                "/System/Library/Fonts/HelveticaNeue.ttc".into(),
            ],
            vec![
                "/System/Library/Fonts/SFNSMono.ttf".into(),
                "/System/Library/Fonts/Menlo.ttc".into(),
            ],
        )
    } else {
        let roots = [
            "/usr/share/fonts/truetype",
            "/usr/share/fonts",
            "/usr/share/fonts/TTF",
        ];
        let find = |names: &[&str]| -> Vec<String> {
            names
                .iter()
                .flat_map(|name| roots.iter().map(move |root| format!("{root}/{name}")))
                .collect()
        };
        (
            find(&[
                "noto/NotoSans-Regular.ttf",
                "google-noto/NotoSans-Regular.ttf",
                "dejavu/DejaVuSans.ttf",
                "DejaVuSans.ttf",
                "ubuntu/Ubuntu-R.ttf",
            ]),
            find(&[
                "noto/NotoSans-SemiBold.ttf",
                "google-noto/NotoSans-SemiBold.ttf",
                "dejavu/DejaVuSans-Bold.ttf",
                "DejaVuSans-Bold.ttf",
                "ubuntu/Ubuntu-M.ttf",
            ]),
            find(&[
                "noto/NotoSansMono-Regular.ttf",
                "dejavu/DejaVuSansMono.ttf",
                "DejaVuSansMono.ttf",
                "ubuntu/UbuntuMono-R.ttf",
            ]),
        )
    };
    let load = |candidates: &[String]| candidates.iter().find_map(|path| std::fs::read(path).ok());

    let mut fonts = FontDefinitions::default();
    let defaults = fonts
        .families
        .get(&FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    let mut add = |name: &str, bytes: Option<Vec<u8>>, family: FontFamily| {
        if let Some(bytes) = bytes {
            fonts
                .font_data
                .insert(name.into(), Arc::new(FontData::from_owned(bytes)));
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, name.into());
        }
    };
    add("system-ui", load(&regular), FontFamily::Proportional);
    add("system-mono", load(&mono), FontFamily::Monospace);

    let strong = FontFamily::Name("strong".into());
    let mut strong_stack = defaults;
    if let Some(bytes) = load(&semibold) {
        fonts.font_data.insert(
            "system-strong".into(),
            Arc::new(FontData::from_owned(bytes)),
        );
        strong_stack.insert(0, "system-strong".into());
    } else if fonts.font_data.contains_key("system-ui") {
        strong_stack.insert(0, "system-ui".into());
    }
    fonts.families.insert(strong, strong_stack);
    ctx.set_fonts(fonts);
}
