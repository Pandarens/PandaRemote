//! Look of the Android app: the desktop app's palette and drawn icons, with touch-sized controls.

use eframe::egui::{self, Color32, Pos2, Rect, Sense, Vec2};
use panda_rc::icons::{self, Icon};

pub const BG: Color32 = Color32::from_rgb(0x0E, 0x10, 0x14);
pub const SURFACE: Color32 = Color32::from_rgb(0x16, 0x19, 0x20);
pub const TOOLBAR: Color32 = Color32::from_rgb(0x12, 0x15, 0x1B);
pub const BORDER: Color32 = Color32::from_rgb(0x22, 0x27, 0x31);
pub const RAISED: Color32 = Color32::from_rgb(0x1D, 0x21, 0x2A);
pub const RAISED_BORDER: Color32 = Color32::from_rgb(0x2A, 0x30, 0x3B);
pub const HOVER: Color32 = Color32::from_rgb(0x26, 0x2B, 0x36);
pub const TEXT: Color32 = Color32::from_rgb(0xE9, 0xEC, 0xF1);
pub const SOFT: Color32 = Color32::from_rgb(0xC9, 0xCE, 0xD6);
pub const MUTED: Color32 = Color32::from_rgb(0x9B, 0xA3, 0xAF);
pub const OFFLINE: Color32 = Color32::from_rgb(0x5C, 0x63, 0x70);
pub const ACCENT: Color32 = Color32::from_rgb(0x33, 0x66, 0xE6);
pub const GREEN: Color32 = Color32::from_rgb(0x3D, 0xD6, 0x8C);
pub const YELLOW: Color32 = Color32::from_rgb(0xFF, 0xD4, 0x3B);
pub const RED: Color32 = Color32::from_rgb(0xFF, 0x6B, 0x6B);
const DANGER_FILL: Color32 = Color32::from_rgb(0x3A, 0x1D, 0x22);
const DANGER_TEXT: Color32 = Color32::from_rgb(0xFF, 0x8A, 0x8A);

const SEMIBOLD: &str = "semibold";

pub fn apply_style(ctx: &egui::Context) {
    install_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = SURFACE;
    v.extreme_bg_color = BG;
    v.selection.bg_fill = ACCENT;
    let w = &mut v.widgets;
    for (state, fill, stroke, fg) in [
        (&mut w.noninteractive, SURFACE, BORDER, TEXT),
        (&mut w.inactive, RAISED, RAISED_BORDER, SOFT),
        (&mut w.hovered, HOVER, RAISED_BORDER, TEXT),
        (&mut w.active, HOVER, RAISED_BORDER, TEXT),
        (&mut w.open, HOVER, RAISED_BORDER, TEXT),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = egui::Stroke::new(1.0, stroke);
        state.fg_stroke = egui::Stroke::new(1.0, fg);
        state.corner_radius = 10.into();
    }
    ctx.set_visuals(v);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = Vec2::new(10.0, 8.0);
        style.spacing.interact_size.y = 44.0;
        use egui::{FontFamily, FontId, TextStyle};
        style.text_styles = [
            (TextStyle::Small, FontId::new(13.0, FontFamily::Proportional)),
            (TextStyle::Body, FontId::new(15.5, FontFamily::Proportional)),
            (TextStyle::Button, FontId::new(15.5, FontFamily::Proportional)),
            (TextStyle::Heading, FontId::new(20.0, FontFamily::Name(SEMIBOLD.into()))),
            (TextStyle::Monospace, FontId::new(14.0, FontFamily::Monospace)),
        ]
        .into();
    });
}

/// Roboto from the system (it has Cyrillic), egui's fonts as fallback.
fn install_fonts(ctx: &egui::Context) {
    use egui::{FontData, FontDefinitions, FontFamily};
    let mut fonts = FontDefinitions::default();
    let mut load = |name: &str, files: &[&str]| {
        let data = files.iter().find_map(|f| std::fs::read(format!("/system/fonts/{f}")).ok())?;
        fonts.font_data.insert(name.into(), std::sync::Arc::new(FontData::from_owned(data)));
        Some(name.to_string())
    };
    let regular = load("roboto", &["Roboto-Regular.ttf", "RobotoStatic-Regular.ttf", "Roboto.ttf"]);
    let medium = load("roboto-medium", &["Roboto-Medium.ttf", "RobotoStatic-Medium.ttf"]);
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    if let Some(r) = &regular {
        prop.insert(0, r.clone());
    }
    let strong: Vec<String> = medium.into_iter().chain(prop.iter().cloned()).collect();
    fonts.families.insert(FontFamily::Name(SEMIBOLD.into()), strong);
    ctx.set_fonts(fonts);
}

pub fn semibold(text: impl Into<String>, size: f32) -> egui::RichText {
    egui::RichText::new(text).family(egui::FontFamily::Name(SEMIBOLD.into())).size(size).color(TEXT)
}

pub fn muted(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).color(MUTED)
}

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Primary,
    Secondary,
    Ghost,
    Active,
    Danger,
}

pub fn button(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, kind: Kind) -> egui::Response {
    button_sized(ui, icon, text, kind, 44.0)
}

/// Touch-sized button: optional icon, text, one of the `Kind` styles.
pub fn button_sized(ui: &mut egui::Ui, icon: Option<Icon>, text: &str, kind: Kind, height: f32) -> egui::Response {
    let galley = ui.painter().layout_no_wrap(text.to_owned(), egui::FontId::proportional(15.5), Color32::PLACEHOLDER);
    let icon_size = 20.0;
    let icon_w = if icon.is_some() { icon_size } else { 0.0 };
    let gap = if icon.is_some() && !text.is_empty() { 8.0 } else { 0.0 };
    let pad = if text.is_empty() { (height - icon_size) / 2.0 } else { 16.0 };
    let width = pad * 2.0 + icon_w + gap + galley.size().x;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click());
    let pressed = resp.is_pointer_button_down_on();
    let none = egui::Stroke::NONE;
    let (fill, stroke, fg) = match kind {
        Kind::Primary => (if pressed { Color32::from_rgb(0x44, 0x76, 0xF0) } else { ACCENT }, none, Color32::WHITE),
        Kind::Secondary => (if pressed { HOVER } else { RAISED }, egui::Stroke::new(1.0, RAISED_BORDER), SOFT),
        Kind::Ghost => (if pressed { HOVER } else { Color32::TRANSPARENT }, none, SOFT),
        Kind::Active => (ACCENT, none, Color32::WHITE),
        Kind::Danger => (DANGER_FILL, none, DANGER_TEXT),
    };
    ui.painter().rect(rect, 10.0, fill, stroke, egui::StrokeKind::Inside);
    let mut x = rect.min.x + pad;
    if let Some(icon) = icon {
        let r = Rect::from_min_size(Pos2::new(x, rect.center().y - icon_size / 2.0), Vec2::splat(icon_size));
        icons::paint(ui, icon, r, icon_size, fg);
        x += icon_size + gap;
    }
    ui.painter().galley(Pos2::new(x, rect.center().y - galley.size().y / 2.0), galley, fg);
    resp
}

pub fn icon(ui: &mut egui::Ui, icon: Icon, color: Color32, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    icons::paint(ui, icon, rect, size, color);
}

pub fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(SURFACE)
        .stroke(egui::Stroke::new(1.0, BORDER))
        .corner_radius(14.0)
        .inner_margin(16.0)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(6.0);
}

pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    ui.painter().circle_filled(rect.center(), 5.0, color);
}
