//! Line icons drawn from small SVGs (24 grid, 1.75 stroke), rasterized once per size and tinted when painted.

use std::{collections::HashMap, sync::Arc};

use eframe::egui::{self, Color32, Rect, TextureHandle, TextureOptions};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Icon {
    Panda,
    Devices,
    Laptop,
    Monitor,
    Gamepad,
    Plus,
    Cursor,
    Pen,
    Laser,
    Eraser,
    Expand,
    Close,
    Copy,
    Eye,
    EyeOff,
    Shield,
    Refresh,
    Power,
    Lock,
    Key,
    Pin,
    More,
    Chevron,
    ChevronDown,
    Rename,
    Trash,
    Check,
    Download,
    Chat,
    Record,
    Wake,
}

const LINE: &str = r#"fill="none" stroke="white" stroke-width="1.75" stroke-linecap="round" stroke-linejoin="round""#;

impl Icon {
    fn body(self) -> String {
        let line = |inner: &str| format!("<g {LINE}>{inner}</g>");
        match self {
            Icon::Panda => concat!(
                r##"<circle cx="6" cy="6.5" r="2.6" fill="#E9ECF1"/><circle cx="18" cy="6.5" r="2.6" fill="#E9ECF1"/>"##,
                r##"<circle cx="12" cy="13" r="7.5" fill="#E9ECF1"/><ellipse cx="9.2" cy="12.6" rx="1.8" ry="2.3" fill="#0E1014"/>"##,
                r##"<ellipse cx="14.8" cy="12.6" rx="1.8" ry="2.3" fill="#0E1014"/><circle cx="12" cy="16.2" r="1.1" fill="#0E1014"/>"##
            )
            .into(),
            Icon::Devices => line(
                r#"<rect x="2" y="4" width="15" height="11" rx="2"/><path d="M6 19h7M9.5 15v4"/><rect x="18" y="8" width="4" height="11" rx="1"/>"#,
            ),
            Icon::Laptop => line(r#"<rect x="4" y="5" width="16" height="10.5" rx="1.5"/><path d="M2 19.5h20"/>"#),
            Icon::Monitor => line(r#"<rect x="3" y="4" width="18" height="12" rx="2"/><path d="M8 20h8M12 16v4"/>"#),
            Icon::Gamepad => line(concat!(
                r#"<path d="M7 8.5h10a4 4 0 0 1 3.9 4.9l-.7 3.1a2.4 2.4 0 0 1-4.2 1l-1.8-2h-4.4l-1.8 2a2.4 2.4 0 0 1-4.2-1l-.7-3.1A4 4 0 0 1 7 8.5z"/>"#,
                r#"<path d="M8 11v3.5M6.25 12.75h3.5"/><circle cx="15.5" cy="12" r="0.4" stroke-width="1.6"/><circle cx="17.5" cy="14" r="0.4" stroke-width="1.6"/>"#
            )),
            Icon::Plus => line(r#"<path d="M12 5v14M5 12h14" stroke-width="2"/>"#),
            Icon::Cursor => line(r#"<path d="M5 3L18.5 10.5L12.5 12.5L10.5 18.5Z"/>"#),
            Icon::Pen => line(r#"<path d="M4 20l1-4L16 5l3 3L8 19z"/><path d="M14 7l3 3"/>"#),
            Icon::Laser => line(r#"<circle cx="12" cy="12" r="3"/><path d="M12 2.5v3M12 18.5v3M2.5 12h3M18.5 12h3"/>"#),
            Icon::Eraser => line(
                r#"<path d="M8 20h12"/><path d="M4.5 15.5l9-9a2 2 0 0 1 2.8 0l2.2 2.2a2 2 0 0 1 0 2.8L11 19H8z"/><path d="M9 11l4 4"/>"#,
            ),
            Icon::Expand => line(r#"<path d="M4 9V4h5M20 9V4h-5M4 15v5h5M20 15v5h-5"/>"#),
            Icon::Close => line(r#"<path d="M6 6l12 12M18 6L6 18" stroke-width="2"/>"#),
            Icon::Copy => line(
                r#"<rect x="8" y="8" width="12" height="12" rx="2"/><path d="M16 8V6a2 2 0 0 0-2-2H6a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h2"/>"#,
            ),
            Icon::Eye => {
                line(r#"<path d="M2 12s3.5-6.5 10-6.5S22 12 22 12s-3.5 6.5-10 6.5S2 12 2 12z"/><circle cx="12" cy="12" r="2.75"/>"#)
            }
            Icon::EyeOff => line(
                r#"<path d="M2 12s3.5-6.5 10-6.5S22 12 22 12s-3.5 6.5-10 6.5S2 12 2 12z"/><circle cx="12" cy="12" r="2.75"/><path d="M4 20L20 4"/>"#,
            ),
            Icon::Shield => line(r#"<path d="M12 3l7 3v5c0 4.5-3 8-7 10-4-2-7-5.5-7-10V6z"/><path d="M9 12l2 2 4-4"/>"#),
            Icon::Refresh => line(r#"<path d="M20 12a8 8 0 1 1-2.3-5.6"/><path d="M20 4v4h-4"/>"#),
            Icon::Power => line(r#"<path d="M12 3v8"/><path d="M6.3 7.3a8 8 0 1 0 11.4 0"/>"#),
            Icon::Lock => line(r#"<rect x="5" y="11" width="14" height="9" rx="2"/><path d="M8 11V8a4 4 0 0 1 8 0v3"/>"#),
            Icon::Key => line(r#"<circle cx="8" cy="15" r="4"/><path d="M11 12l9-9M17 6l3 3"/>"#),
            Icon::Pin => line(concat!(
                r#"<rect x="2.5" y="6.5" width="19" height="11" rx="2.5"/>"#,
                r#"<circle cx="7.5" cy="12" r="0.6" stroke-width="2"/><circle cx="12" cy="12" r="0.6" stroke-width="2"/>"#,
                r#"<circle cx="16.5" cy="12" r="0.6" stroke-width="2"/>"#
            )),
            Icon::More => {
                r#"<g fill="white"><circle cx="6" cy="12" r="1.6"/><circle cx="12" cy="12" r="1.6"/><circle cx="18" cy="12" r="1.6"/></g>"#
                    .into()
            }
            Icon::Chevron => line(r#"<path d="M9 6l6 6-6 6"/>"#),
            Icon::ChevronDown => line(r#"<path d="M6 9l6 6 6-6"/>"#),
            Icon::Rename => line(r#"<path d="M4 20h4L19 9l-4-4L4 16z"/>"#),
            Icon::Download => line(r#"<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>"#),
            Icon::Chat => line(r#"<path d="M4 5h16v11H9l-5 4z"/><path d="M8 9.5h8M8 12.5h5"/>"#),
            Icon::Record => r#"<circle cx="12" cy="12" r="7" fill="none" stroke="white" stroke-width="1.75"/><circle cx="12" cy="12" r="3.5" fill="white"/>"#.into(),
            Icon::Wake => line(r#"<path d="M12 3v3M12 18v3M3 12h3M18 12h3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1"/><circle cx="12" cy="12" r="3.5"/>"#),
            Icon::Check => line(r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#),
            Icon::Trash => line(r#"<path d="M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3"/>"#),
        }
    }

    /// RGBA (premultiplied) of the icon at `px`×`px`.
    fn rasterize(self, px: u32) -> egui::ColorImage {
        use resvg::{tiny_skia, usvg};
        let svg = format!(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24">{}</svg>"#, self.body());
        let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("icon size");
        if let Ok(tree) = usvg::Tree::from_str(&svg, &usvg::Options::default()) {
            let scale = px as f32 / 24.0;
            resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        }
        egui::ColorImage::from_rgba_premultiplied([px as usize, px as usize], pixmap.data())
    }
}

/// The app icon (white panda on a dark rounded square), RGBA straight alpha, for the tray.
pub fn app_icon_rgba(px: u32) -> Vec<u8> {
    use resvg::{tiny_skia, usvg};
    let svg = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="0.5" y="0.5" width="23" height="23" rx="6" fill="#1D212A"/><g transform="translate(2.4 2.4) scale(0.8)">{}</g></svg>"##,
        Icon::Panda.body()
    );
    let mut pixmap = tiny_skia::Pixmap::new(px, px).expect("icon size");
    if let Ok(tree) = usvg::Tree::from_str(&svg, &usvg::Options::default()) {
        let s = px as f32 / 24.0;
        resvg::render(&tree, tiny_skia::Transform::from_scale(s, s), &mut pixmap.as_mut());
    }
    pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.red(), c.green(), c.blue(), c.alpha()]
        })
        .collect()
}

#[derive(Clone, Default)]
struct Cache(Arc<std::sync::Mutex<HashMap<(Icon, u32), TextureHandle>>>);

/// The icon's texture at the screen's pixel density for a `size`-point square.
fn texture(ctx: &egui::Context, icon: Icon, size: f32) -> TextureHandle {
    let px = (size * ctx.pixels_per_point()).round().max(1.0) as u32;
    let cache = ctx.data_mut(|d| d.get_temp_mut_or_default::<Cache>(egui::Id::unique("panda_icons")).clone());
    let mut map = cache.0.lock().unwrap();
    map.entry((icon, px))
        .or_insert_with(|| ctx.load_texture(format!("icon-{icon:?}-{px}"), icon.rasterize(px), TextureOptions::LINEAR))
        .clone()
}

/// Paints `icon` centered in `rect` at `size` points, in `color` (the panda keeps its own colours).
pub fn paint(ui: &egui::Ui, icon: Icon, rect: Rect, size: f32, color: Color32) {
    let tex = texture(ui.ctx(), icon, size);
    let tint = if icon == Icon::Panda { Color32::WHITE } else { color };
    let r = Rect::from_center_size(rect.center(), egui::Vec2::splat(size));
    ui.painter().image(tex.id(), r, Rect::from_min_max(egui::Pos2::ZERO, egui::Pos2::new(1.0, 1.0)), tint);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_draws_something() {
        for icon in [
            Icon::Panda,
            Icon::Devices,
            Icon::Laptop,
            Icon::Monitor,
            Icon::Gamepad,
            Icon::Plus,
            Icon::Cursor,
            Icon::Pen,
            Icon::Laser,
            Icon::Eraser,
            Icon::Expand,
            Icon::Close,
            Icon::Copy,
            Icon::Eye,
            Icon::EyeOff,
            Icon::Shield,
            Icon::Refresh,
            Icon::Power,
            Icon::Lock,
            Icon::Key,
            Icon::Pin,
            Icon::More,
            Icon::Chevron,
            Icon::ChevronDown,
            Icon::Rename,
            Icon::Trash,
            Icon::Check,
            Icon::Download,
            Icon::Chat,
            Icon::Record,
            Icon::Wake,
        ] {
            let img = icon.rasterize(32);
            let ink = img.pixels.iter().filter(|p| p.a() > 0).count();
            assert!(ink > 20, "{icon:?} is empty");
        }
    }
}
