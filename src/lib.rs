//! Panda Remote as a library, shared by the desktop program (`src/main.rs`) and the Android viewer (`android/`).

pub mod client;
pub mod config;
pub mod icons;
pub mod media;
pub mod proto;
pub mod release;
pub mod ticket;

// The host, the desktop window and the Windows integration are not part of the Android viewer.
#[cfg(not(target_os = "android"))]
pub mod autostart;
#[cfg(not(target_os = "android"))]
pub mod gui;
#[cfg(not(target_os = "android"))]
pub mod host;
#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod tray;
#[cfg(not(target_os = "android"))]
pub mod update;
#[cfg(not(target_os = "android"))]
pub mod yuv_render;
