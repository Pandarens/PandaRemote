//! Tray icon: closing the window hides it to the tray; clicking the icon brings it back, «Выход» quits.
//! The window is shown and hidden with Win32 calls, because eframe runs no frames for a hidden window
//! (a request to show it from the app itself would never be processed).

use anyhow::Result;
use tray_icon::{
    MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
};
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{IsIconic, SW_HIDE, SW_RESTORE, SW_SHOW, SetForegroundWindow, ShowWindow},
};

pub struct Tray {
    _icon: TrayIcon,
    hwnd: isize,
}

impl Tray {
    /// Creates the icon for the main window `hwnd`.
    pub fn new(hwnd: isize, ctx: eframe::egui::Context) -> Result<Self> {
        let open = MenuItem::new("Открыть Panda", true, None);
        let quit = MenuItem::new("Выход", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit])?;
        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        let icon = tray_icon::Icon::from_rgba(crate::icons::app_icon_rgba(32), 32, 32)?;
        let tray =
            TrayIconBuilder::new().with_menu(Box::new(menu)).with_tooltip("Panda Remote").with_icon(icon).build()?;
        let menu_ctx = ctx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                show(hwnd);
                menu_ctx.request_repaint();
            } else if e.id == quit_id {
                tracing::info!("quit from the tray");
                std::process::exit(0);
            }
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => {
                show(hwnd);
                ctx.request_repaint();
            }
            _ => {}
        }));
        Ok(Tray { _icon: tray, hwnd })
    }

    pub fn hide_window(&self) {
        unsafe {
            let _ = ShowWindow(HWND(self.hwnd as _), SW_HIDE);
        }
    }
}

fn show(hwnd: isize) {
    let hwnd = HWND(hwnd as _);
    unsafe {
        let _ = ShowWindow(hwnd, if IsIconic(hwnd).as_bool() { SW_RESTORE } else { SW_SHOW });
        let _ = SetForegroundWindow(hwnd);
    }
}
