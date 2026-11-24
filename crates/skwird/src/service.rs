//! The system service on Windows (`docs/DESIGN.md`, decisions 1 and 3;
//! `docs/PLAN.md`, phase 4, step 7): `skwird serve --system`, started by
//! the service control manager at boot, as `LocalSystem`.
//!
//! - **Registered** by `skwird service add`, which starts it, as
//!   `systemctl enable --now` does; `remove` stops it and takes it back.
//!   It starts once the boot is done (a delayed automatic start), is
//!   started again after a failure, and keeps only the privileges it
//!   uses, as `CapabilityBoundingSet` keeps a Linux service's.
//! - **Run** under the manager's dispatcher: running once its pipe is
//!   made; told to stop, it says it stopped, and the process ends once the
//!   dispatcher gives back, as a Linux service ends on `SIGTERM`. Run from
//!   a console, it serves in the foreground: by hand, or in a test, as an
//!   administrator.
//! - **Its folders**, in the machine's program data, are its alone and
//!   the administrators' ([`make_own`]): any user may make a folder there,
//!   and one another user made is refused.

#![allow(unsafe_code)] // The service manager and security descriptors have no safe binding; each block says why it is sound.

use std::ffi::{OsStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CALL_NOT_IMPLEMENTED,
    ERROR_FAILED_SERVICE_CONTROLLER_CONNECT, ERROR_SERVICE_DOES_NOT_EXIST,
    ERROR_SERVICE_NOT_ACTIVE, ERROR_SERVICE_SPECIFIC_ERROR, GetLastError, HANDLE,
    INVALID_HANDLE_VALUE, LocalFree, NO_ERROR,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows_sys::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, OWNER_SECURITY_INFORMATION,
    PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
};
use windows_sys::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetFileInformationByHandle,
    OPEN_EXISTING, READ_CONTROL, WRITE_DAC,
};
use windows_sys::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW, DeleteService,
    OpenSCManagerW, OpenServiceW, QueryServiceStatus, RegisterServiceCtrlHandlerExW, SC_ACTION,
    SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_CONNECT, SC_MANAGER_CREATE_SERVICE,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
    SERVICE_CONTROL_DEVICEEVENT, SERVICE_CONTROL_INTERROGATE, SERVICE_CONTROL_SHUTDOWN,
    SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO, SERVICE_DESCRIPTIONW,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS,
    SERVICE_REQUIRED_PRIVILEGES_INFOW, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STOP, SERVICE_STOP_PENDING, SERVICE_STOPPED,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};

use crate::engine::{Config, log};

/// The service's name, the machine's: one system service per machine.
pub const NAME: &str = "skwird";
/// `DELETE`, the standard right to delete an object.
const DELETE: u32 = 0x0001_0000;
/// `LocalSystem`, which the service runs as; the administrators, who may
/// run it by hand.
const SYSTEM: &str = "S-1-5-18";
const ADMINISTRATORS: &str = "S-1-5-32-544";

/// The privileges it keeps (`SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO`);
/// the manager takes every other from its token:
/// - to read any folder's descriptor and records, whatever its own access
///   says (`skwird_crawl::read_anything`);
/// - to pass through any folder, which every user has;
/// - to make a volume's change journal where it has none.
const PRIVILEGES: &[&str] = &[
    "SeBackupPrivilege",
    "SeChangeNotifyPrivilege",
    "SeManageVolumePrivilege",
];

fn wide(text: impl AsRef<OsStr>) -> Vec<u16> {
    text.as_ref().encode_wide().chain([0]).collect()
}

fn last_error(what: &str) -> String {
    format!("{what}: {}", std::io::Error::last_os_error())
}

/// A handle of the service manager's, closed once dropped.
struct Service(SC_HANDLE);

impl Drop for Service {
    fn drop(&mut self) {
        // SAFETY: a handle the manager gave, closed once.
        unsafe { CloseServiceHandle(self.0) };
    }
}

/// The configuration [`dispatch`] serves with, for its service's main.
static CONFIG: Mutex<Option<Config>> = Mutex::new(None);
/// The handle the manager is told the service's state through.
static STATUS: AtomicUsize = AtomicUsize::new(0);
/// The pipe the service answers on, which a stop wakes its loop through.
static PIPE: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
/// How long a stop may take: the writer applies what it heard and saves
/// every root.
const STOPPING_FOR: u32 = 30_000;

/// Serve `config` as the service the manager started: false, at once,
/// where this process is not one it started (run from a console). Gives
/// back once the service has stopped.
pub fn dispatch(config: &Config) -> Result<bool, String> {
    *CONFIG.lock().unwrap_or_else(PoisonError::into_inner) = Some(config.clone());
    *PIPE.lock().unwrap_or_else(PoisonError::into_inner) = Some(config.dirs.socket());
    let mut name = wide(NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: name.as_mut_ptr(),
            lpServiceProc: Some(service_main),
        },
        SERVICE_TABLE_ENTRYW {
            lpServiceName: std::ptr::null_mut(),
            lpServiceProc: None,
        },
    ];
    // SAFETY: the table, its last entry empty, alive until the dispatcher
    // returns, once every service of the process has stopped.
    if unsafe { windows_sys::Win32::System::Services::StartServiceCtrlDispatcherW(table.as_ptr()) }
        != 0
    {
        return Ok(true);
    }
    // SAFETY: no argument.
    match unsafe { GetLastError() } {
        ERROR_FAILED_SERVICE_CONTROLLER_CONNECT => {
            CONFIG.lock().unwrap_or_else(PoisonError::into_inner).take();
            Ok(false)
        }
        _ => Err(last_error("cannot reach the service manager")),
    }
}

/// Tell the manager the service is in `state`, having ended with `code`
/// where it stopped.
fn report(state: SERVICE_STATUS_CURRENT_STATE, code: u32) {
    let handle = STATUS.load(Ordering::Acquire);
    if handle == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: if state == SERVICE_RUNNING {
            SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN
        } else {
            0
        },
        dwWin32ExitCode: if code == 0 {
            NO_ERROR
        } else {
            ERROR_SERVICE_SPECIFIC_ERROR
        },
        dwServiceSpecificExitCode: code,
        dwCheckPoint: 0,
        dwWaitHint: if state == SERVICE_STOP_PENDING {
            STOPPING_FOR
        } else {
            0
        },
    };
    // SAFETY: the handle the manager gave this service, which it never
    // closes; the status read for the call.
    unsafe {
        windows_sys::Win32::System::Services::SetServiceStatus(
            handle as *mut c_void,
            &raw const status,
        )
    };
}

/// The service's main, on a thread the dispatcher makes: its handler
/// registered, its folders and log made, then the engine served.
unsafe extern "system" fn service_main(_: u32, _: *mut windows_sys::core::PWSTR) {
    let name = wide(NAME);
    // SAFETY: a NUL-terminated name, the handler, no context; the handle
    // given is the manager's, never closed.
    let handle =
        unsafe { RegisterServiceCtrlHandlerExW(name.as_ptr(), Some(control), std::ptr::null()) };
    if handle.is_null() {
        return;
    }
    STATUS.store(handle as usize, Ordering::Release);
    hear_volumes(handle);
    let Some(config) = CONFIG.lock().unwrap_or_else(PoisonError::into_inner).take() else {
        report(SERVICE_STOPPED, 1);
        return;
    };
    // The manager gives the service no standard error: its log is in its
    // state folder.
    if crate::dirs::make_system(&config.dirs).is_ok() {
        let _ = crate::os::log_to(&config.dirs.state.join("engine.log"));
    }
    // Below normal, as the user's task runs: a crawl yields to what the
    // users are doing.
    // SAFETY: this process's pseudo-handle; a class Windows defines.
    unsafe {
        windows_sys::Win32::System::Threading::SetPriorityClass(
            windows_sys::Win32::System::Threading::GetCurrentProcess(),
            windows_sys::Win32::System::Threading::BELOW_NORMAL_PRIORITY_CLASS,
        )
    };
    report(SERVICE_RUNNING, 0);
    match crate::serve::serve(&config) {
        Ok(()) => report(SERVICE_STOPPED, 0),
        Err(e) => {
            log(&e);
            report(SERVICE_STOPPED, 1);
        }
    }
}

/// What the manager asks of the service: to stop, as the machine shuts
/// down or an administrator asks. It says it is stopping, and its loop is
/// woken to end as when idle (`crate::serve::stop`): the writer applies
/// what it heard and saves every root, its journal's position among it,
/// so that the next start resumes from there (DESIGN 2). Once the engine
/// is served, the service says it stopped, the dispatcher gives back
/// ([`dispatch`]), and the process ends with `main`. Ended here, the
/// process would end before the manager's call returned, which then fails
/// (`ERROR_BROKEN_PIPE`), as a runner's first stop did.
unsafe extern "system" fn control(code: u32, event: u32, data: *mut c_void, _: *mut c_void) -> u32 {
    match code {
        // SAFETY: what the manager gives with a device's event.
        SERVICE_CONTROL_DEVICEEVENT => unsafe { device_event(event, data) },
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            log("stopped by the service manager");
            report(SERVICE_STOP_PENDING, 0);
            let pipe = PIPE.lock().unwrap_or_else(PoisonError::into_inner).clone();
            match pipe {
                Some(pipe) => crate::serve::stop(&pipe),
                None => report(SERVICE_STOPPED, 0),
            }
            NO_ERROR
        }
        SERVICE_CONTROL_INTERROGATE => NO_ERROR,
        _ => ERROR_CALL_NOT_IMPLEMENTED,
    }
}

/// `arg` as a command line takes it back whole (the C runtime's rules,
/// which std's arguments are read by): quoted where it holds a space, a
/// tab or a quote, its backslashes before a quote doubled.
pub fn quote(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut out = String::from('"');
    let mut slashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => slashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', slashes * 2 + 1));
                out.push('"');
                slashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', slashes));
                out.push(c);
                slashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', slashes * 2));
    out.push('"');
    out
}

/// The service's command line: this program, `serve --system
/// --forever`, then `args` (roots named, threads).
pub fn command_line(program: &Path, args: &[String]) -> String {
    let mut line = quote(&program.to_string_lossy());
    line.push_str(" serve --system --forever");
    for a in args {
        line.push(' ');
        line.push_str(&quote(a));
    }
    line
}

/// Register the service, this program serving with `args` beside
/// `--system --forever`, as `LocalSystem`, and start it.
pub fn add(args: &[String]) -> Result<String, String> {
    let program = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    let line = wide(command_line(&program, args));
    // SAFETY: this machine's manager, its active database; the handle
    // given is ours to close.
    let manager = unsafe {
        OpenSCManagerW(
            std::ptr::null(),
            std::ptr::null(),
            SC_MANAGER_CONNECT | SC_MANAGER_CREATE_SERVICE,
        )
    };
    if manager.is_null() {
        return Err(last_error(
            "cannot reach the service manager (an administrator may)",
        ));
    }
    let manager = Service(manager);
    let name = wide(NAME);
    // SAFETY: the manager's handle; NUL-terminated strings alive for the
    // call; no group, tag or dependencies; no account named, so
    // `LocalSystem`, with no password. The handle given is ours to close.
    let service = unsafe {
        CreateServiceW(
            manager.0,
            name.as_ptr(),
            name.as_ptr(),
            SERVICE_ALL_ACCESS,
            SERVICE_WIN32_OWN_PROCESS,
            SERVICE_AUTO_START,
            SERVICE_ERROR_NORMAL,
            line.as_ptr(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if service.is_null() {
        return Err(last_error("cannot register the service"));
    }
    let service = Service(service);
    configure(&service)?;
    // SAFETY: the service's handle, opened to start it; no arguments.
    if unsafe { StartServiceW(service.0, 0, std::ptr::null()) } == 0 {
        return Err(last_error("registered, the service cannot start"));
    }
    Ok(format!("the service {NAME} is registered and started"))
}

/// The service's description, its start delayed until the boot is done,
/// started again after a failure, and the privileges it keeps.
fn configure(service: &Service) -> Result<(), String> {
    let set = |level, info: *const c_void, what: &str| {
        // SAFETY: the service's handle, opened to change it; the structure
        // the level names, alive for the call.
        if unsafe { ChangeServiceConfig2W(service.0, level, info) } == 0 {
            Err(last_error(what))
        } else {
            Ok(())
        }
    };
    let mut text = wide(
        "The system's index of file names kept current: every user answered what they could list.",
    );
    let description = SERVICE_DESCRIPTIONW {
        lpDescription: text.as_mut_ptr(),
    };
    set(
        SERVICE_CONFIG_DESCRIPTION,
        (&raw const description).cast(),
        "cannot describe the service",
    )?;
    let delayed = SERVICE_DELAYED_AUTO_START_INFO {
        fDelayedAutostart: 1,
    };
    set(
        SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
        (&raw const delayed).cast(),
        "cannot delay the service's start",
    )?;
    // Started again 10 s after each of its first three failures in a day.
    let mut actions = [SC_ACTION {
        Type: SC_ACTION_RESTART,
        Delay: 10_000,
    }; 3];
    let failures = SERVICE_FAILURE_ACTIONSW {
        dwResetPeriod: 24 * 3600,
        lpRebootMsg: std::ptr::null_mut(),
        lpCommand: std::ptr::null_mut(),
        cActions: 3,
        lpsaActions: actions.as_mut_ptr(),
    };
    set(
        SERVICE_CONFIG_FAILURE_ACTIONS,
        (&raw const failures).cast(),
        "cannot have the service started again after a failure",
    )?;
    // Each privilege's name NUL-terminated, an empty one last.
    let mut names: Vec<u16> = PRIVILEGES
        .iter()
        .flat_map(|p| p.encode_utf16().chain([0]))
        .chain([0])
        .collect();
    let privileges = SERVICE_REQUIRED_PRIVILEGES_INFOW {
        pmszRequiredPrivileges: names.as_mut_ptr(),
    };
    set(
        SERVICE_CONFIG_REQUIRED_PRIVILEGES_INFO,
        (&raw const privileges).cast(),
        "cannot keep the service to its privileges",
    )
}

/// Stop the service, waited for at most 30 s, and take it back.
pub fn remove() -> Result<String, String> {
    // SAFETY: this machine's manager, its active database; the handle
    // given is ours to close.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return Err(last_error(
            "cannot reach the service manager (an administrator may)",
        ));
    }
    let manager = Service(manager);
    let name = wide(NAME);
    // SAFETY: the manager's handle; a NUL-terminated name; the handle
    // given is ours to close.
    let service = unsafe {
        OpenServiceW(
            manager.0,
            name.as_ptr(),
            SERVICE_STOP | SERVICE_QUERY_STATUS | DELETE,
        )
    };
    if service.is_null() {
        // SAFETY: no argument.
        if unsafe { GetLastError() } == ERROR_SERVICE_DOES_NOT_EXIST {
            return Ok(format!("no service {NAME} is registered"));
        }
        return Err(last_error("cannot open the service"));
    }
    let service = Service(service);
    let mut status: SERVICE_STATUS = SERVICE_STATUS::default();
    // SAFETY: the service's handle, opened to stop it; the status ours.
    if unsafe { ControlService(service.0, SERVICE_CONTROL_STOP, &raw mut status) } == 0
        // SAFETY: no argument.
        && unsafe { GetLastError() } != ERROR_SERVICE_NOT_ACTIVE
    {
        return Err(last_error("cannot stop the service"));
    }
    // A stop the command waits for, bounded: no engine's loop.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        // SAFETY: the service's handle, opened to query it; the status ours.
        if unsafe { QueryServiceStatus(service.0, &raw mut status) } == 0 {
            return Err(last_error("cannot ask the service's state"));
        }
        if status.dwCurrentState == SERVICE_STOPPED {
            break;
        }
        if Instant::now() > deadline {
            return Err("the service did not stop within 30 s".into());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // SAFETY: the service's handle, opened to delete it.
    if unsafe { DeleteService(service.0) } == 0 {
        return Err(last_error("cannot take the service back"));
    }
    Ok(format!("the service {NAME} is stopped and taken back"))
}

/// Whether the service is registered and running, for a test.
pub fn running() -> bool {
    // SAFETY: this machine's manager, its active database; the handle
    // given is ours to close.
    let manager = unsafe { OpenSCManagerW(std::ptr::null(), std::ptr::null(), SC_MANAGER_CONNECT) };
    if manager.is_null() {
        return false;
    }
    let manager = Service(manager);
    let name = wide(NAME);
    // SAFETY: as above, the service opened to query it.
    let service = unsafe { OpenServiceW(manager.0, name.as_ptr(), SERVICE_QUERY_STATUS) };
    if service.is_null() {
        return false;
    }
    let service = Service(service);
    let mut status = SERVICE_STATUS::default();
    // SAFETY: the service's handle, opened to query it; the status ours.
    let asked = unsafe { QueryServiceStatus(service.0, &raw mut status) } != 0;
    asked && status.dwCurrentState == SERVICE_RUNNING
}

/// A security descriptor a conversion made, freed once dropped.
pub(crate) struct Descriptor(pub(crate) PSECURITY_DESCRIPTOR);

impl Drop for Descriptor {
    fn drop(&mut self) {
        // SAFETY: memory the system gave, freed once.
        unsafe { LocalFree(self.0.cast()) };
    }
}

/// A handle of ours, closed once dropped.
struct Owned(HANDLE);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: a handle of ours, closed once.
        unsafe { CloseHandle(self.0) };
    }
}

/// The text of the SID at `sid`.
pub(crate) fn sid_text(sid: PSID) -> Option<String> {
    let mut text: *mut u16 = std::ptr::null_mut();
    // SAFETY: a SID the system gave, valid while its descriptor is; the
    // string given is ours to free.
    if unsafe { ConvertSidToStringSidW(sid, &raw mut text) } == 0 {
        return None;
    }
    // SAFETY: a NUL-terminated string the call made, read to its NUL; a
    // SID's text is under 200 units.
    let len = (0..512)
        .take_while(|&i| unsafe { *text.add(i) } != 0)
        .count();
    // SAFETY: `len` units, read before the string is freed.
    let sid = String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(text, len) });
    // SAFETY: the string the call made, freed once.
    unsafe { LocalFree(text.cast()) };
    Some(sid)
}

/// `dir` made the service's own: `LocalSystem`'s and the administrators'
/// alone, inherited by what is made in it, nothing inherited from above.
/// Made so if it is not there; one there already is taken only if it is
/// a folder, no link, owned by `LocalSystem` or the administrators, and
/// is made theirs alone again. Any user may make a folder in the
/// machine's program data: one another user made is refused, since its
/// maker could read what the service keeps there.
pub fn make_own(dir: &Path) -> Result<(), String> {
    let sddl = wide("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
    let mut security: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated string; the descriptor given is ours to
    // free.
    if unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &raw mut security,
            std::ptr::null_mut(),
        )
    } == 0
    {
        return Err(last_error("cannot make a folder's security descriptor"));
    }
    let security = Descriptor(security);
    let attributes = SECURITY_ATTRIBUTES {
        nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>()).unwrap_or(u32::MAX),
        lpSecurityDescriptor: security.0,
        bInheritHandle: 0,
    };
    let name = skwird_crawl::verbatim(dir);
    // SAFETY: a NUL-terminated name; the attributes and their descriptor
    // alive for the call.
    if unsafe { CreateDirectoryW(name.as_ptr(), &raw const attributes) } != 0 {
        return Ok(());
    }
    // SAFETY: no argument.
    if unsafe { GetLastError() } != ERROR_ALREADY_EXISTS {
        return Err(last_error("cannot make a folder of skwird's"));
    }
    take_own(dir, &name, &security)
}

/// The folder `dir` (`name`, its verbatim form), there already, taken as
/// [`make_own`] says, and given the access list of `security`.
fn take_own(dir: &Path, name: &[u16], security: &Descriptor) -> Result<(), String> {
    // SAFETY: a NUL-terminated name alive for the call; the folder itself,
    // never what a link there leads to; the handle given is ours to close.
    let handle = unsafe {
        CreateFileW(
            name.as_ptr(),
            READ_CONTROL | WRITE_DAC,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(last_error("cannot open a folder of skwird's"));
    }
    let handle = Owned(handle);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle, open; the structure ours.
    if unsafe { GetFileInformationByHandle(handle.0, &raw mut info) } == 0 {
        return Err(last_error("cannot stat a folder of skwird's"));
    }
    if info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
        || info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
    {
        return Err("a folder of skwird's is not a folder, or is a link: refused".into());
    }
    let mut owner: PSID = std::ptr::null_mut();
    let mut held: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: the handle, opened to read its security; the owner points
    // into the descriptor given, which is ours to free.
    let failed = unsafe {
        GetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &raw mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut held,
        )
    };
    if failed != 0 {
        return Err(format!(
            "cannot read a folder of skwird's owner: {}",
            std::io::Error::from_raw_os_error(i32::try_from(failed).unwrap_or(i32::MAX))
        ));
    }
    let held = Descriptor(held);
    let owner = sid_text(owner);
    drop(held);
    if !owner
        .as_deref()
        .is_some_and(|o| o == SYSTEM || o == ADMINISTRATORS)
    {
        return Err(format!(
            "{} is not the service's own, nor the administrators': refused (remove it, as an \
             administrator)",
            dir.display()
        ));
    }
    let mut present = 0;
    let mut defaulted = 0;
    let mut dacl: *mut ACL = std::ptr::null_mut();
    // SAFETY: the descriptor made above; the list points into it.
    if unsafe {
        GetSecurityDescriptorDacl(
            security.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    } == 0
    {
        return Err(last_error("cannot read a folder's security descriptor"));
    }
    // SAFETY: the handle, opened to write its access list; the list,
    // alive while the descriptor is; nothing else changed.
    let failed = unsafe {
        SetSecurityInfo(
            handle.0,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
        )
    };
    if failed != 0 {
        return Err(format!(
            "cannot make a folder of skwird's the service's alone: {}",
            std::io::Error::from_raw_os_error(i32::try_from(failed).unwrap_or(i32::MAX))
        ));
    }
    Ok(())
}

/// Every volume's arrival and removal, told the service by the manager
/// (`SERVICE_CONTROL_DEVICEEVENT`): through `status`, its handle, for as
/// long as it runs.
fn hear_volumes(status: windows_sys::Win32::System::Services::SERVICE_STATUS_HANDLE) {
    use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_VOLUME;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DBT_DEVTYP_DEVICEINTERFACE, DEV_BROADCAST_DEVICEINTERFACE_W, DEVICE_NOTIFY_SERVICE_HANDLE,
        RegisterDeviceNotificationW,
    };
    let filter = DEV_BROADCAST_DEVICEINTERFACE_W {
        dbcc_size: u32::try_from(size_of::<DEV_BROADCAST_DEVICEINTERFACE_W>()).unwrap_or(0),
        dbcc_devicetype: DBT_DEVTYP_DEVICEINTERFACE,
        dbcc_reserved: 0,
        dbcc_classguid: GUID_DEVINTERFACE_VOLUME,
        dbcc_name: [0],
    };
    // SAFETY: the service's status handle, the manager's; the filter read
    // for the call. The registration is kept as long as the process runs.
    let notify = unsafe {
        RegisterDeviceNotificationW(
            status,
            (&raw const filter).cast(),
            DEVICE_NOTIFY_SERVICE_HANDLE,
        )
    };
    if notify.is_null() {
        log(&last_error(
            "volumes arriving or going are not heard, roots are those at the start",
        ));
    }
}

/// Each registration on a root's volume ([`hold_volume`]), and its root.
static HELD: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());

/// A root's volume, a handle on it registered to tell the service when
/// its removal is asked ([`Held`]).
pub struct Held {
    notify: windows_sys::Win32::UI::WindowsAndMessaging::HDEVNOTIFY,
    /// Closed as its removal is asked ([`Held::release`]), or after the
    /// registration ends.
    volume: Option<std::os::windows::io::OwnedHandle>,
}

impl Held {
    /// Its handle closed, its registration kept: ended from within the
    /// manager's call that asks the removal, it could wait for that call
    /// to end, which waits for this. Dropped later, it ends.
    pub fn release(&mut self) {
        self.volume = None;
    }

    pub fn released(&self) -> bool {
        self.volume.is_none()
    }
}

// SAFETY: a registration and a handle, the process's, used by the thread
// holding them alone.
unsafe impl Send for Held {}

impl Drop for Held {
    fn drop(&mut self) {
        HELD.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|&(n, _)| n != self.notify as usize);
        // SAFETY: the registration made, ended once; the handle closed with
        // `volume`, after it.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::UnregisterDeviceNotification(self.notify)
        };
    }
}

/// The volume of root `root`, at `path`, held to be told when its removal
/// is asked, so that the root lets go of it first and the removal goes on
/// (a volume a handle is open on is in use). Under the service manager
/// alone: none from a console, which nothing tells.
pub fn hold_volume(root: usize, path: &Path) -> Option<Held> {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DBT_DEVTYP_HANDLE, DEV_BROADCAST_HANDLE, DEVICE_NOTIFY_SERVICE_HANDLE,
        RegisterDeviceNotificationW,
    };
    let status = STATUS.load(Ordering::Acquire);
    if status == 0 {
        return None;
    }
    let volume = skwird_crawl::journal::Volume::handle_of(path).ok()?;
    let filter = DEV_BROADCAST_HANDLE {
        dbch_size: u32::try_from(size_of::<DEV_BROADCAST_HANDLE>()).unwrap_or(0),
        dbch_devicetype: DBT_DEVTYP_HANDLE,
        dbch_handle: std::os::windows::io::AsRawHandle::as_raw_handle(&volume),
        ..DEV_BROADCAST_HANDLE::default()
    };
    // SAFETY: the service's status handle; the filter, naming a handle of
    // ours, read for the call.
    let notify = unsafe {
        RegisterDeviceNotificationW(
            status as *mut c_void,
            (&raw const filter).cast(),
            DEVICE_NOTIFY_SERVICE_HANDLE,
        )
    };
    if notify.is_null() {
        log(&last_error(&format!(
            "root {root}: its volume's removal is not heard"
        )));
        return None;
    }
    HELD.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((notify as usize, root));
    Some(Held {
        notify,
        volume: Some(volume),
    })
}

/// A device's event (`SERVICE_CONTROL_DEVICEEVENT`):
/// - a volume arrived, or went: the engine follows its roots again
///   (`crate::serve::volumes_changed`);
/// - the removal of a root's volume asked, pending or done: the root lets
///   go of it before this answers, so that the removal goes on (a virtual
///   disk detached does not ask, and is told as it goes); refused by
///   another, it is followed again.
///
/// # Safety
///
/// `data` is what the manager gives with the event: a device broadcast's
/// header, and the rest of its kind.
unsafe fn device_event(event: u32, data: *mut c_void) -> u32 {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DBT_DEVICEARRIVAL, DBT_DEVICEQUERYREMOVE, DBT_DEVICEQUERYREMOVEFAILED,
        DBT_DEVICEREMOVECOMPLETE, DBT_DEVICEREMOVEPENDING, DBT_DEVTYP_HANDLE, DEV_BROADCAST_HANDLE,
        DEV_BROADCAST_HDR,
    };
    // A root's volume asked to go, going, or gone: let go of it first, as
    // a removal may not ask (a virtual disk detached does not).
    if matches!(
        event,
        DBT_DEVICEQUERYREMOVE | DBT_DEVICEREMOVEPENDING | DBT_DEVICEREMOVECOMPLETE
    ) && !data.is_null()
    {
        // SAFETY: every broadcast begins with its header.
        let header = unsafe { &*data.cast::<DEV_BROADCAST_HDR>() };
        if header.dbch_devicetype == DBT_DEVTYP_HANDLE {
            // SAFETY: a handle's broadcast, as its header says.
            let handle = unsafe { &*data.cast::<DEV_BROADCAST_HANDLE>() };
            let root = HELD
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .find(|&&(n, _)| n == handle.dbch_hdevnotify as usize)
                .map(|&(_, root)| root);
            if let Some(root) = root {
                crate::serve::volume_leaving(root);
            }
        }
    }
    if matches!(
        event,
        DBT_DEVICEARRIVAL | DBT_DEVICEREMOVECOMPLETE | DBT_DEVICEQUERYREMOVEFAILED
    ) {
        crate::serve::volumes_changed();
    }
    NO_ERROR
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An argument reads back whole: a root's `\` before the closing quote
    /// doubled, a quote escaped, plain ones as they are.
    #[test]
    fn a_command_line_reads_back_each_argument_whole() {
        assert_eq!(quote(r"S:\"), r"S:\");
        assert_eq!(
            quote(r"C:\Program Files\skwird\"),
            r#""C:\Program Files\skwird\\""#
        );
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(""), r#""""#);
        assert_eq!(
            command_line(
                Path::new(r"C:\Program Files\skwird\skwird.exe"),
                &["--root".into(), r"D:\My data\".into()]
            ),
            r#""C:\Program Files\skwird\skwird.exe" serve --system --forever --root "D:\My data\\""#
        );
    }
}
