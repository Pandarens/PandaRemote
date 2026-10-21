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
        ServiceStartType, ServiceState, ServiceStatus, ServiceType, UserEventCode,
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

    // Installing over an existing service is an upgrade: stop it first so its program file is free.
    let existing = manager
        .open_service(
            SERVICE_NAME,
            ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::CHANGE_CONFIG,
        )
        .ok();
    if let Some(service) = &existing {
        stop_and_wait(service)?;
    }
    let result = deploy(&manager, existing.as_ref());
    if result.is_err()
        && let Some(service) = &existing
    {
        // Don't leave the PC unreachable: bring the previous version back up.
        let _ = service.start::<&str>(&[]);
    }
    result
}

/// Locks down the data directory, copies the program and host files, then registers and starts the service.
fn deploy(manager: &ServiceManager, existing: Option<&windows_service::service::Service>) -> Result<()> {
    // The service runs as SYSTEM with whatever key and secret it finds here, so the directory must not be
    // writable (or pre-seeded) by ordinary users: take ownership, drop inherited rights, and allow only
    // SYSTEM and Administrators. Then always copy this user's files over (existing tickets keep working).
    let to = config::machine_dir();
    std::fs::create_dir_all(&to).with_context(|| format!("create {}", to.display()))?;
    lock_down(to.parent().unwrap_or(&to))?;
    // Users may pass through the top directory to the logs, which they may read; `config` stays closed.
    icacls(to.parent().unwrap_or(&to), &["/grant", "*S-1-5-32-545:(RX)", "/Q"])?;
    // An earlier version let users list `config`; take that back.
    icacls(&to, &["/remove:g", "*S-1-5-32-545", "/Q"])?;
    let logs = config::machine_logs_dir();
    std::fs::create_dir_all(&logs).with_context(|| format!("create {}", logs.display()))?;
    icacls(&logs, &["/setowner", "*S-1-5-32-544", "/Q"])?;
    icacls(
        &logs,
        &[
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
            "*S-1-5-32-545:(OI)(CI)RX",
            "/Q",
        ],
    )?;
    // The service runs its own copy, so the original can be rebuilt or moved while it runs.
    let exe = install_binary()?;
    let from = config::user_dir();
    for name in config::HOST_FILES {
        let (src, dst) = (from.join(name), to.join(name));
        let _ = std::fs::remove_file(&dst);
        if src.exists() {
            std::fs::copy(&src, &dst).with_context(|| format!("copy {name}"))?;
        }
    }

    // Ctrl+Alt+Del from the viewer: Windows lets services press it only when this policy allows them to.
    if let Err(e) = allow_service_sas() {
        tracing::warn!("Ctrl+Alt+Del will not work: {e:#}");
    }

    let info = ServiceInfo {
        name: SERVICE_NAME.into(),
        display_name: "Panda Remote".into(),
        service_type: SERVICE_TYPE,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: exe,
        launch_arguments: vec!["service".into(), "run".into()],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    let created;
    let service = match existing {
        Some(service) => {
            service.change_config(&info).context("не удалось обновить службу")?;
            service
        }
        None => {
            created = manager
                .create_service(&info, ServiceAccess::CHANGE_CONFIG | ServiceAccess::START)
                .context("не удалось создать службу")?;
            &created
        }
    };
    let _ = service.set_description("Удалённый доступ Panda Remote: экран блокировки, UAC, работа до входа в систему");
    service.start::<&str>(&[]).context("служба создана, но не запустилась")?;
    Ok(())
}

/// Stops the service and waits (up to 10 s) until it reports Stopped, so its program file is released.
fn stop_and_wait(service: &windows_service::service::Service) -> Result<()> {
    if service.query_status()?.current_state == ServiceState::Stopped {
        return Ok(());
    }
    let _ = service.stop();
    for _ in 0..100 {
        if service.query_status()?.current_state == ServiceState::Stopped {
            // The process may still be exiting for a moment after reporting Stopped.
            std::thread::sleep(Duration::from_millis(500));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("служба не остановилась за 10 секунд")
}

/// Owner = Administrators, no inheritance, full control for SYSTEM and Administrators only (by SID, so it
/// works on localized Windows). The directories themselves must succeed; their contents are best effort,
/// since a log may still be held by a service that is just exiting (the key and secret are recreated anyway).
fn lock_down(root: &std::path::Path) -> Result<()> {
    const ADMINS: &str = "*S-1-5-32-544";
    for dir in [root.to_path_buf(), config::machine_dir()] {
        icacls(&dir, &["/setowner", ADMINS, "/Q"])?;
        icacls(&dir, &["/inheritance:r", "/grant:r", "*S-1-5-18:(OI)(CI)F", "*S-1-5-32-544:(OI)(CI)F", "/Q"])?;
    }
    let _ = icacls(root, &["/setowner", ADMINS, "/T", "/C", "/Q"]);
    Ok(())
}

/// Sets the `SoftwareSASGeneration` policy to "services" (1) unless it already allows them (1 or 3).
fn allow_service_sas() -> Result<()> {
    const KEY: &str = r"HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\Policies\System";
    let query = std::process::Command::new("reg").args(["query", KEY, "/v", "SoftwareSASGeneration"]).output()?;
    let current = String::from_utf8_lossy(&query.stdout);
    if current.contains("0x1") || current.contains("0x3") {
        return Ok(());
    }
    let out = std::process::Command::new("reg")
        .args(["add", KEY, "/v", "SoftwareSASGeneration", "/t", "REG_DWORD", "/d", "1", "/f"])
        .output()
        .context("reg")?;
    if !out.status.success() {
        bail!("reg add failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(())
}

fn icacls(path: &std::path::Path, args: &[&str]) -> Result<()> {
    let out = std::process::Command::new("icacls").arg(path).args(args).output().context("icacls")?;
    if !out.status.success() {
        bail!("icacls {:?} failed: {}", args, String::from_utf8_lossy(&out.stdout));
    }
    Ok(())
}

/// Copies the running executable to `%ProgramFiles%\PandaRC\panda-rc.exe` (admin-only, like any program there).
fn install_binary() -> Result<std::path::PathBuf> {
    let target = install_binary_path()?;
    std::fs::create_dir_all(target.parent().unwrap()).with_context(|| format!("create {}", target.display()))?;
    let current = std::env::current_exe()?;
    if current == target {
        return Ok(target);
    }
    let dir = target.parent().unwrap().to_path_buf();
    // Leftovers from earlier upgrades whose processes have exited by now.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with("panda-rc.old-") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    // A program that is still running (the stopping service or its helper) can't be overwritten,
    // but it can be renamed: move it aside and put the new one in its place.
    if target.exists() && std::fs::remove_file(&target).is_err() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let aside = dir.join(format!("panda-rc.old-{stamp}.exe"));
        std::fs::rename(&target, &aside).with_context(|| format!("move busy {} aside", target.display()))?;
    }
    std::fs::copy(&current, &target).with_context(|| format!("copy program to {}", target.display()))?;
    Ok(target)
}

fn install_binary_path() -> Result<std::path::PathBuf> {
    let dir = std::env::var_os("ProgramFiles")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(r"C:\Program Files"))
        .join("PandaRC");
    Ok(dir.join("panda-rc.exe"))
}

pub fn uninstall() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("нужны права администратора")?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE | ServiceAccess::QUERY_STATUS)
        .context("служба не установлена")?;
    stop_and_wait(&service)?;
    service.delete()?;
    // The installed copy is no longer in use (best effort: it may still be shutting down).
    if let Ok(exe) = install_binary_path()
        && exe != std::env::current_exe()?
    {
        let _ = std::fs::remove_file(exe);
    }
    Ok(())
}

/// Service control code the session helper sends to have the service press Ctrl+Alt+Del.
const SAS_EVENT: UserEventCode = unsafe { UserEventCode::from_unchecked(130) };

/// Asks the service to press Ctrl+Alt+Del. Windows lets only a service do that (`SendSAS`), and the host
/// runs in a helper process in the user's session, so it passes the request on.
pub fn request_sas() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .context("Ctrl+Alt+Del работает только со службой Windows")?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::USER_DEFINED_CONTROL)
        .context("Ctrl+Alt+Del работает только со службой Windows")?;
    service.notify(SAS_EVENT)?;
    Ok(())
}

/// Presses Ctrl+Alt+Del (the secure attention sequence) from the service.
fn send_sas() {
    use windows::{
        Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW},
        core::{s, w},
    };
    unsafe {
        let Ok(lib) = LoadLibraryW(w!("sas.dll")) else {
            tracing::warn!("sas.dll not found");
            return;
        };
        let Some(proc) = GetProcAddress(lib, s!("SendSAS")) else {
            tracing::warn!("SendSAS not found");
            return;
        };
        let send_sas: unsafe extern "system" fn(i32) = std::mem::transmute(proc);
        // FALSE: called by a service.
        send_sas(0);
    }
    tracing::info!("Ctrl+Alt+Del sent");
}

/// Copies the service's host files into a user's directory `to` (the elevated copy may run as another
/// account, so the app names its own), so the app can show the ticket and the PIN state without admin rights.
pub fn sync_user_copy(to: &std::path::Path) -> Result<()> {
    let from = config::machine_dir();
    std::fs::create_dir_all(to).with_context(|| format!("create {}", to.display()))?;
    for name in config::HOST_FILES {
        std::fs::copy(from.join(name), to.join(name)).with_context(|| format!("copy {name}"))?;
    }
    Ok(())
}

/// Sets the service's PIN from `pin_file` (empty: no PIN), deletes the file and refreshes the copy in `to`.
pub fn set_pin(pin_file: &std::path::Path, to: &std::path::Path) -> Result<()> {
    let pin = std::fs::read_to_string(pin_file).with_context(|| format!("read {}", pin_file.display()));
    let _ = std::fs::remove_file(pin_file);
    let pin = pin?;
    let pin = pin.trim();
    config::HostConfig::load_or_create()?.set_pin((!pin.is_empty()).then_some(pin))?;
    sync_user_copy(to)
}

/// Finishes an install/uninstall: prints on success, and mirrors the outcome into `result_file`
/// for [`run_elevated`] (the elevated process has no console the GUI could read).
pub fn report(result_file: Option<std::path::PathBuf>, result: Result<()>, on_ok: impl FnOnce()) -> Result<()> {
    if let Some(path) = result_file {
        let text = match &result {
            Ok(()) => "ok".to_string(),
            Err(e) => format!("{e:#}"),
        };
        let _ = std::fs::write(path, text);
    }
    result.map(|()| on_ok())
}

/// Runs `panda-rc service <action>` with administrator rights (UAC prompt) and waits for it.
pub fn run_elevated(action: &str) -> Result<()> {
    use windows::Win32::{
        Foundation::{ERROR_CANCELLED, WAIT_FAILED},
        System::Threading::{GetExitCodeProcess, INFINITE},
        UI::{
            Shell::{SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW},
            WindowsAndMessaging::SW_HIDE,
        },
    };
    let result_file = std::env::temp_dir().join(format!("panda-service-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&result_file);
    let wide = |s: &str| s.encode_utf16().chain([0]).collect::<Vec<u16>>();
    let exe = wide(&std::env::current_exe()?.to_string_lossy());
    let params = wide(&format!("service {action} --result-file \"{}\"", result_file.display()));
    let verb = wide("runas");
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            if e.code() == ERROR_CANCELLED.to_hresult() {
                bail!("отменено в окне контроля учётных записей");
            }
            return Err(e).context("не удалось запустить с правами администратора");
        }
        if WaitForSingleObject(info.hProcess, INFINITE) == WAIT_FAILED {
            bail!("ожидание процесса установки не удалось");
        }
        let mut code = 0u32;
        let _ = GetExitCodeProcess(info.hProcess, &mut code);
        let _ = CloseHandle(info.hProcess);
        let text = std::fs::read_to_string(&result_file).unwrap_or_default();
        let _ = std::fs::remove_file(&result_file);
        match text.as_str() {
            "ok" => Ok(()),
            "" => bail!("процесс завершился с кодом {code} без отчёта"),
            err => bail!("{err}"),
        }
    }
}

/// Stops and starts the service again (run as a separate process by the self-updater).
pub fn restart() -> Result<()> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?;
    let service =
        manager.open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::START | ServiceAccess::QUERY_STATUS)?;
    stop_and_wait(&service)?;
    service.start::<&str>(&[])?;
    Ok(())
}

/// Checks GitHub for a signed release every 6 hours (first time shortly after start). When one is
/// found it replaces the installed program and restarts the service from a separate process.
fn spawn_updater() {
    let _ = std::thread::Builder::new().name("updater".into()).spawn(|| {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        std::thread::sleep(Duration::from_secs(120));
        loop {
            match rt.block_on(update_once()) {
                Ok(true) => return, // restarting into the new version
                Ok(false) => {}
                Err(e) => tracing::warn!("update check failed: {e:#}"),
            }
            std::thread::sleep(crate::update::CHECK_INTERVAL);
        }
    });
}

async fn update_once() -> Result<bool> {
    let Some(rel) = crate::update::newer_release().await? else { return Ok(false) };
    tracing::info!(version = %rel.version, "new release, downloading");
    let staged = crate::update::download(&rel, &crate::update::staging_dir(true)).await?;
    let exe = std::env::current_exe()?;
    crate::update::install(&staged, &exe)?;
    tracing::info!(version = %rel.version, "update installed, restarting the service");
    std::process::Command::new(&exe).args(["service", "restart"]).spawn()?;
    Ok(true)
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
        ServiceControl::UserEvent(code) if code == SAS_EVENT => {
            send_sas();
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
    tracing::info!(version = %crate::update::Version::current(), "service started");
    spawn_updater();

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
