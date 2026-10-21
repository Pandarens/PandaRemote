//! Keeps a thread attached to the desktop that currently receives input.
//!
//! Windows shows the lock screen and UAC prompts on separate desktops (`Winlogon`, secure desktop).
//! A process running as SYSTEM in the console session (the service helper) can capture and inject
//! there, but only after moving its threads onto that desktop. For a normal user process opening
//! those desktops is denied and this is a no-op.

use std::time::{Duration, Instant};

use windows::Win32::{
    Foundation::{GENERIC_ALL, HANDLE},
    System::{
        StationsAndDesktops::{
            CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetThreadDesktop, GetUserObjectInformationW,
            HDESK, OpenInputDesktop, SetThreadDesktop, UOI_NAME,
        },
        Threading::GetCurrentThreadId,
    },
};

/// Name of the desktop that currently receives input (`Default`, `Winlogon`, ...); `None` when this
/// process may not open it (a normal user process while the lock screen is up).
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)).ok()?;
        let name = desktop_name(desk);
        let _ = CloseDesktop(desk);
        name
    }
}

/// True while this process's Windows session is locked. On Windows 11 the lock screen is drawn on the
/// normal `Default` desktop, so the desktop name alone doesn't reveal it.
pub fn session_locked() -> bool {
    use windows::Win32::System::{
        RemoteDesktop::{
            ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTSFreeMemory, WTSINFOEXW, WTSQuerySessionInformationW,
            WTSSessionInfoEx,
        },
        Threading::GetCurrentProcessId,
    };
    const WTS_SESSIONSTATE_LOCK: i32 = 0;
    unsafe {
        let mut session = 0u32;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_err() {
            return false;
        }
        let mut buf = windows::core::PWSTR::null();
        let mut len = 0u32;
        if WTSQuerySessionInformationW(Some(WTS_CURRENT_SERVER_HANDLE), session, WTSSessionInfoEx, &mut buf, &mut len)
            .is_err()
            || buf.is_null()
        {
            return false;
        }
        let info = &*(buf.0 as *const WTSINFOEXW);
        let locked = info.Level == 1 && info.Data.WTSInfoExLevel1.SessionFlags == WTS_SESSIONSTATE_LOCK;
        WTSFreeMemory(buf.0.cast());
        locked
    }
}

/// Locks the console session (back to the lock screen). Falls back to disconnecting the session,
/// which also ends at the lock screen, if `LockWorkStation` is refused.
pub fn lock_session() {
    use windows::Win32::System::{
        RemoteDesktop::{ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTSDisconnectSession},
        Shutdown::LockWorkStation,
        Threading::GetCurrentProcessId,
    };
    unsafe {
        match LockWorkStation() {
            Ok(()) => tracing::info!("session locked again"),
            Err(e) => {
                tracing::warn!("LockWorkStation failed ({e}), disconnecting the session instead");
                let mut session = 0u32;
                if ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_ok()
                    && let Err(e) = WTSDisconnectSession(Some(WTS_CURRENT_SERVER_HANDLE), session, false)
                {
                    tracing::warn!("WTSDisconnectSession failed: {e}");
                }
            }
        }
    }
}

/// Name of the desktop the calling thread is attached to.
pub fn thread_desktop_name() -> Option<String> {
    unsafe { GetThreadDesktop(GetCurrentThreadId()).ok().and_then(desktop_name) }
}

/// Moves the calling thread to the input desktop if it changed. Returns true on a switch.
pub fn follow_input_desktop() -> bool {
    unsafe {
        let Ok(input) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(GENERIC_ALL.0)) else {
            return false;
        };
        let input_name = desktop_name(input);
        let current_name = GetThreadDesktop(GetCurrentThreadId()).ok().and_then(desktop_name);
        if input_name.is_none() || input_name == current_name {
            let _ = CloseDesktop(input);
            return false;
        }
        // The handle stays open while the thread is attached to it; switches are rare.
        match SetThreadDesktop(input) {
            Ok(()) => {
                tracing::info!(desktop = input_name.as_deref().unwrap_or("?"), "switched to input desktop");
                true
            }
            Err(e) => {
                tracing::debug!("SetThreadDesktop failed: {e}");
                let _ = CloseDesktop(input);
                false
            }
        }
    }
}

fn desktop_name(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Rate-limited [`follow_input_desktop`] for hot paths such as input injection.
pub struct DesktopFollower {
    last: Option<Instant>,
}

impl DesktopFollower {
    pub const fn new() -> Self {
        DesktopFollower { last: None }
    }

    pub fn check(&mut self) {
        if self.last.is_none_or(|t| t.elapsed() > Duration::from_millis(200)) {
            self.last = Some(Instant::now());
            follow_input_desktop();
        }
    }
}
