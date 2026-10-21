//! On-screen drawing for the remote user: a transparent, click-through, topmost window covering the
//! primary monitor, rendered with tiny-skia and composited by Windows (`UpdateLayeredWindow`).
//! The window is excluded from screen capture, because the viewer already draws the same strokes itself.
//! It also shows short notes from the viewer ("сеанс записывается", a typed message) at the top.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use tiny_skia::{Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform};
use windows::{
    Win32::{
        Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, SIZE, WPARAM},
        Graphics::Gdi::{
            AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, CreateCompatibleDC,
            CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, HBITMAP, HDC, ReleaseDC, SelectObject,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::*,
    },
    core::w,
};

use crate::media::annotate::{DrawEvent, Ink, STROKE_WIDTH};

/// What the overlay shows.
pub enum Overlay {
    Draw(DrawEvent),
    /// A note at the top of the screen for a few seconds.
    Note(String),
}

/// How long a note stays, and its fade-out at the end.
const NOTE_TIME: Duration = Duration::from_secs(6);
const NOTE_FADE: Duration = Duration::from_millis(600);

/// Starts the overlay thread; it lives until the sender is dropped.
pub fn spawn() -> mpsc::Sender<Overlay> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("overlay".into()).spawn(move || {
        if let Err(e) = run(rx) {
            tracing::warn!("drawing overlay unavailable: {e:#}");
        }
    });
    tx
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

struct Surface {
    hwnd: HWND,
    screen: HDC,
    mem: HDC,
    bitmap: HBITMAP,
    bits: *mut u8,
    width: i32,
    height: i32,
}

impl Surface {
    fn new() -> anyhow::Result<Self> {
        unsafe {
            let (width, height) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSEXW {
                cbSize: size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(wnd_proc),
                hInstance: instance.into(),
                lpszClassName: w!("PandaRemoteOverlay"),
                ..Default::default()
            };
            RegisterClassExW(&class);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                w!("PandaRemoteOverlay"),
                w!(""),
                WS_POPUP,
                0,
                0,
                width,
                height,
                None,
                None,
                Some(instance.into()),
                None,
            )?;
            // The viewer draws its own strokes; capturing ours too would double them. PANDA_OVERLAY_CAPTURE=1
            // keeps them in the stream (used to test the overlay end to end).
            if std::env::var_os("PANDA_OVERLAY_CAPTURE").is_none() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // top-down rows
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let bitmap = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)?;
            SelectObject(mem, bitmap.into());
            Ok(Surface { hwnd, screen, mem, bitmap, bits: bits.cast(), width, height })
        }
    }

    /// Copies a premultiplied RGBA pixmap into the window (Windows wants premultiplied BGRA).
    fn present(&mut self, pixmap: &Pixmap) {
        let len = (self.width * self.height * 4) as usize;
        let dst = unsafe { std::slice::from_raw_parts_mut(self.bits, len) };
        for (d, s) in dst.chunks_exact_mut(4).zip(pixmap.data().chunks_exact(4)) {
            d.copy_from_slice(&[s[2], s[1], s[0], s[3]]);
        }
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let size = SIZE { cx: self.width, cy: self.height };
        let origin = POINT { x: 0, y: 0 };
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                Some(self.screen),
                Some(&origin),
                Some(&size),
                Some(self.mem),
                Some(&origin),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            // Stay above windows that were raised since.
            let _ = SetWindowPos(self.hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    fn pump_messages(&self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.mem);
            ReleaseDC(None, self.screen);
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn render(ink: &Ink, now: Instant, pixmap: &mut Pixmap) {
    pixmap.fill(Color::TRANSPARENT);
    let (w, h) = (pixmap.width() as f32, pixmap.height() as f32);
    let width = (STROKE_WIDTH * h).max(2.0);
    let line = Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Default::default() };
    let mut paint = Paint { anti_alias: true, ..Default::default() };
    for s in &ink.strokes {
        let alpha = (Ink::stroke_alpha(s, now) * 255.0) as u8;
        paint.set_color_rgba8(s.color[0], s.color[1], s.color[2], alpha);
        let mut pb = PathBuilder::new();
        let (x0, y0) = s.points[0];
        if s.points.len() == 1 {
            if let Some(dot) = PathBuilder::from_circle(x0 * w, y0 * h, width / 2.0) {
                pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
            }
            continue;
        }
        pb.move_to(x0 * w, y0 * h);
        for &(x, y) in &s.points[1..] {
            pb.line_to(x * w, y * h);
        }
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(&path, &paint, &line, Transform::identity(), None);
        }
    }
    for (a, b, alpha) in ink.laser_trail(now) {
        paint.set_color_rgba8(255, 40, 40, (alpha * 160.0) as u8);
        let mut pb = PathBuilder::new();
        pb.move_to(a.0 * w, a.1 * h);
        pb.line_to(b.0 * w, b.1 * h);
        if let Some(path) = pb.finish() {
            pixmap.stroke_path(
                &path,
                &paint,
                &Stroke { width: width * 1.6, ..line.clone() },
                Transform::identity(),
                None,
            );
        }
    }
    if let Some(((x, y), alpha)) = ink.laser_dot(now) {
        for (radius, a) in [(width * 4.0, 0.25), (width * 2.0, 1.0)] {
            paint.set_color_rgba8(255, 40, 40, (alpha * a * 255.0) as u8);
            if let Some(dot) = PathBuilder::from_circle(x * w, y * h, radius) {
                pixmap.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
            }
        }
    }
}

fn run(rx: mpsc::Receiver<Overlay>) -> anyhow::Result<()> {
    let mut surface = Surface::new()?;
    let mut pixmap =
        Pixmap::new(surface.width as u32, surface.height as u32).ok_or_else(|| anyhow::anyhow!("pixmap"))?;
    let mut ink = Ink::default();
    let font = load_font();
    let mut note: Option<(String, Instant)> = None;
    let mut shown = false;
    loop {
        surface.pump_messages();
        // Animate at ~60 fps while something is visible; otherwise sleep until the next event.
        let first = if shown { rx.recv_timeout(Duration::from_millis(16)) } else { rx.recv().map_err(Into::into) };
        let now = Instant::now();
        let mut apply = |cmd: Overlay| match cmd {
            Overlay::Draw(ev) => ink.apply(&ev, now),
            Overlay::Note(text) => note = Some((text, now)),
        };
        match first {
            Ok(cmd) => apply(cmd),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(cmd) = rx.try_recv() {
            apply(cmd);
        }
        if note.as_ref().is_some_and(|(_, at)| now.duration_since(*at) > NOTE_TIME) {
            note = None;
        }
        let visible = ink.prune(now) | note.is_some();
        if visible || shown {
            render(&ink, now, &mut pixmap);
            if let (Some((text, at)), Some(font)) = (&note, &font) {
                let left = NOTE_TIME.saturating_sub(now.duration_since(*at));
                let alpha = (left.as_secs_f32() / NOTE_FADE.as_secs_f32()).min(1.0);
                draw_note(&mut pixmap, font, text, alpha);
            }
            surface.present(&pixmap);
        }
        shown = visible;
    }
    Ok(())
}

fn load_font() -> Option<ab_glyph::FontVec> {
    let dir =
        std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    ["seguisb.ttf", "segoeui.ttf", "arial.ttf"]
        .iter()
        .find_map(|f| ab_glyph::FontVec::try_from_vec(std::fs::read(dir.join(f)).ok()?).ok())
}

/// A dark rounded box with white text, centred near the top of the screen.
fn draw_note(pixmap: &mut Pixmap, font: &ab_glyph::FontVec, text: &str, alpha: f32) {
    use ab_glyph::{Font, PxScale, ScaleFont, point};
    let scale = PxScale::from((pixmap.height() as f32 / 42.0).clamp(18.0, 34.0));
    let sf = font.as_scaled(scale);
    let text: String = text.chars().take(160).collect();
    let mut glyphs = Vec::new();
    let mut x = 0.0f32;
    let mut prev = None;
    for c in text.chars() {
        let id = sf.glyph_id(c);
        if let Some(p) = prev {
            x += sf.kern(p, id);
        }
        glyphs.push(id.with_scale_and_position(scale, point(x, sf.ascent())));
        x += sf.h_advance(id);
        prev = Some(id);
    }
    let (tw, th) = (x.ceil(), (sf.ascent() - sf.descent()).ceil());
    let pad = th * 0.6;
    let (bw, bh) = (tw + pad * 2.0, th + pad * 1.4);
    let (bx, by) = ((pixmap.width() as f32 - bw) / 2.0, pixmap.height() as f32 * 0.06);
    let mut paint = Paint { anti_alias: true, ..Default::default() };
    paint.set_color_rgba8(14, 16, 20, (alpha * 225.0) as u8);
    if let Some(rect) = tiny_skia::Rect::from_xywh(bx, by, bw, bh) {
        let r = bh / 2.0;
        let mut pb = PathBuilder::new();
        pb.push_rect(rect);
        let path = rounded_rect(bx, by, bw, bh, r).or_else(|| pb.finish());
        if let Some(path) = path {
            pixmap.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }
    let (ox, oy) = (bx + pad, by + pad * 0.7);
    let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
    let data = pixmap.data_mut();
    for g in glyphs {
        let Some(outlined) = font.outline_glyph(g) else { continue };
        let bounds = outlined.px_bounds();
        outlined.draw(|gx, gy, coverage| {
            let px = (ox + bounds.min.x) as i32 + gx as i32;
            let py = (oy + bounds.min.y) as i32 + gy as i32;
            if px < 0 || py < 0 || px >= w || py >= h {
                return;
            }
            let a = (coverage * alpha).clamp(0.0, 1.0);
            let i = ((py * w + px) * 4) as usize;
            // Premultiplied "source over" of white.
            for c in 0..4 {
                let dst = data[i + c] as f32;
                data[i + c] = (255.0 * a + dst * (1.0 - a)).round() as u8;
            }
        });
    }
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}
