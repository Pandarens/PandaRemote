//! Windows service: keeps the host reachable at the lock screen, in UAC prompts and before anyone logs in.
//!
//! A service runs in session 0 and cannot see the user's screen, so it only supervises: it starts
//! `panda-rc host --service-helper` as SYSTEM inside the active console session (the token is the
//! service's own, moved to that session) and restarts it when the session changes or the helper exits.
//! Running as SYSTEM in the console session lets the helper follow the input desktop to Winlogon/UAC.

use std::{ffi::OsString, sync::mpsc, time::Duration};

use anyhow::{Context, Result, bail};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0},
        Security::{
            DuplicateTokenEx, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS, TokenPrimary,
            TokenSessionId,
        },
        System::{
            RemoteDesktop::WTSGetActiveConsoleSessionId,
            Threading::{
                CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, PROCESS_INFORMATION,
                STARTUPINFOW, TerminateProcess, WaitForSingleObject,
            },
        },
    },
    core::{PCWSTR, PWSTR},
};
use windows_service::{
    define_windows_service,
    service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo,
        ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
    service_manager::{ServiceManager, ServiceManagerAccess},
};

use crate::config;

pub const SERVICE_NAME: &str = "PandaRemote";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
const NO_SESSION: u32 = 0xFFFF_FFFF;

// ---------- install / uninstall (run elevated) ----------

pub fn install() -> Result<()> {
    // Check for admin rights first, so a failed attempt leaves nothing behind.
    let manager = ServiceManager::local_computer(
        None::<&str>,
        ServiceManagerAccess::CONNECT | ServiceManagerAccess::CREATE_SERVICE,
    )
    .context("нужны права администратора: запустите терминал от имени администратора")?;

    // The service runs as SYSTEM with whatever key and secret it finds here, so the directory must not be
    // writable (or pre-seeded) by ordinary users: take ownership, drop inherited rights, and allow only
    // SYSTEM and Administrators. Then always copy this user's files over (existing tickets keep working).
    let to = config::machine_dir();
    std::fs::create_dir_all(&to).with_context(|| format!("create {}", to.display()))?;
    lock_down(to.parent().unwrap_or(&to))?;
    let from = config::user_dir();
    for name in config::HOST_FILES {
        let (src, dst) = (from.join(name), to.join(name));
        let _ = std::fs::remove_file(&dst);
        if src.exists() {
            std::fs::copy(&src, &dst).with_context(|| format!("copy {name}"))?;
        }
    }

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Panda Remote".into(),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe()?,
        launch_arguments: vec!["service".into(), "run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let service = manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
        .context("не удалось создать службу (уже установлена?)")?;
    let _ = service.set_description("Удалённый доступ Panda Remote: экран блокировки, UAC, работа до входа в систему");
    config::set_service_installed(true)?;
    service.start::<&str>(&[]).context("служба создана, но не запустилась")?;
    Ok(())
}

/// Owner = Administrators, no inheritance, full control for SYSTEM and Administrators only (by SID, so it
/// works on localized Windows).
fn lock_down(dir: &std::path::Path) -> Result<()> {
    let run = |args: &[&str]| -> Result<()> {
        let out = std::process::Command::new("icacls").arg(dir).args(args).output().context("icacls")?;
        if !out.status.success() {
            bail!("icacls {:?} failed: {}", args, String::from_utf8_lossy(&out.stdout));
        }
        Ok(())
    };
    run(&["/setowner", "*S-1-5-32-544", "/T", "/Q"])?;
    run(&["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/T", "/Q"])
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("нужны права администратора")?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
        .context("служба не установлена")?;
    if service.query_status()?.current_state != ServiceState::Stopped {
        let _ = service.stop();
        for _ in 0..50 {
            if service.query_status()?.current_state == ServiceState::Stopped {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    service.delete()?;
    config::set_service_installed(false)?;
    Ok(())
}

/// Service state for the GUI/CLI; `None` when not installed or not queryable.
pub fn status() -> Option<ServiceState> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    let service = manager.open_service(SERVICE_NAME, ServiceAccess::QUERY_STATUS).ok()?;
    Some(service.query_status().ok()?.current_state)
}

// ---------- service process ----------

define_windows_service!(ffi_service_main, service_main);

/// Entry point for `panda-rc service run` (started by the service control manager).
pub fn run_dispatcher() -> Result<()> {
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).context("must be started by the service manager")?;
    Ok(())
}

fn service_main(_args: Vec<OsString>) {
    if let Err(e) = run_service() {
        tracing::error!("service failed: {e:#}");
    }
}

fn run_service() -> Result<()> {
    let (stop_tx, stop_rx) = mpsc::channel();
    let handler = move |event| match event {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let status = service_control_handler::register(SERVICE_NAME, handler)?;
    let report = |state, accept| {
        let _ = status.set_service_status(ServiceStatus {
            service_type: SERVICE_TYPE,
            current_state: state,
            controls_accepted: accept,
            exit_code: ServiceExitCode::Win32(0),
            checkpoint: 0,
            wait_hint: Duration::default(),
            process_id: None,
        });
    };
    report(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN);
    tracing::info!("service started");

    // The helper inherits this and keeps its key, secret and journal in the machine directory.
    unsafe { std::env::set_var("PANDA_HOME", config::machine_dir()) };
    let mut helper: Option<Helper> = None;
    loop {
        if stop_rx.recv_timeout(Duration::from_secs(1)).is_ok() {
            break;
        }
        let session = unsafe { WTSGetActiveConsoleSessionId() };
        let healthy = helper.as_ref().is_some_and(|h| h.session == session && h.running());
        if healthy || session == NO_SESSION {
            continue;
        }
        if let Some(old) = helper.take() {
            tracing::info!(session = old.session, "stopping helper");
        }
        match Helper::spawn(session) {
            Ok(h) => {
                tracing::info!(session, "helper started");
                helper = Some(h);
            }
            Err(e) => {
                tracing::warn!(session, "helper start failed: {e:#}");
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
    drop(helper);
    tracing::info!("service stopped");
    report(ServiceState::Stopped, ServiceControlAccept::empty());
    Ok(())
}

/// The host process running as SYSTEM in a user session.
struct Helper {
    session: u32,
    process: HANDLE,
}

impl Helper {
    fn spawn(session: u32) -> Result<Self> {
        let exe = std::env::current_exe()?;
        let mut cmd: Vec<u16> =
            format!("\"{}\" host --service-helper", exe.display()).encode_utf16().chain([0]).collect();
        let mut desktop: Vec<u16> = "winsta0\\default".encode_utf16().chain([0]).collect();
        unsafe {
            let mut own = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_ALL_ACCESS, &mut own)?;
            let mut token = HANDLE::default();
            let dup = DuplicateTokenEx(own, TOKEN_ALL_ACCESS, None, SecurityImpersonation, TokenPrimary, &mut token);
            let _ = CloseHandle(own);
            dup?;
            let moved = SetTokenInformation(token, TokenSessionId, (&session as *const u32).cast(), 4);
            if let Err(e) = moved {
                let _ = CloseHandle(token);
                bail!("SetTokenInformation: {e}");
            }
            let startup = STARTUPINFOW {
                cb: size_of::<STARTUPINFOW>() as u32,
                lpDesktop: PWSTR(desktop.as_mut_ptr()),
                ..Default::default()
            };
            let mut info = PROCESS_INFORMATION::default();
            let created = CreateProcessAsUserW(
                Some(token),
                PCWSTR::null(),
                Some(PWSTR(cmd.as_mut_ptr())),
                None,
                None,
                false,
                CREATE_NO_WINDOW,
                None,
                PCWSTR::null(),
                &startup,
                &mut info,
            );
            let _ = CloseHandle(token);
            created.context("CreateProcessAsUser")?;
            let _ = CloseHandle(info.hThread);
            Ok(Helper { session, process: info.hProcess })
        }
    }

    fn running(&self) -> bool {
        unsafe { WaitForSingleObject(self.process, 0) != WAIT_OBJECT_0 }
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        unsafe {
            let _ = TerminateProcess(self.process, 0);
            let _ = CloseHandle(self.process);
        }
    }
}
