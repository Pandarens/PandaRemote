//! On-screen drawing for the remote user: a transparent, click-through, topmost window covering the
//! primary monitor, rendered with tiny-skia and composited by Windows (`UpdateLayeredWindow`).
//! The window is excluded from screen capture, because the viewer already draws the same strokes itself.

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

/// Starts the overlay thread; it lives until the sender is dropped.
pub fn spawn() -> mpsc::Sender<DrawEvent> {
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

fn run(rx: mpsc::Receiver<DrawEvent>) -> anyhow::Result<()> {
    let mut surface = Surface::new()?;
    let mut pixmap =
        Pixmap::new(surface.width as u32, surface.height as u32).ok_or_else(|| anyhow::anyhow!("pixmap"))?;
    let mut ink = Ink::default();
    let mut shown = false;
    loop {
        surface.pump_messages();
        // Animate at ~60 fps while something is visible; otherwise sleep until the next event.
        let first = if shown { rx.recv_timeout(Duration::from_millis(16)) } else { rx.recv().map_err(Into::into) };
        let now = Instant::now();
        match first {
            Ok(ev) => ink.apply(&ev, now),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(ev) = rx.try_recv() {
            ink.apply(&ev, now);
        }
        let visible = ink.prune(now);
        if visible || shown {
            render(&ink, now, &mut pixmap);
            surface.present(&pixmap);
        }
        shown = visible;
    }
    Ok(())
}
