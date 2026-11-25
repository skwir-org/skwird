//! Removable volumes on Windows ([`crate::removable`]): those mounted, and
//! what a user's engine hears of them coming, going and being ejected,
//! with no window and no service manager (`CM_Register_Notification`).
//!
//! - **Mounted:** each volume (`FindFirstVolumeW`), at each path it is
//!   mounted at, a letter's or a folder's. Removable where its drive says
//!   so (a stick, a card; an optical disc), or where its disk is on a USB
//!   or an SD card's bus, or is a virtual disk attached (a `.vhdx`, as a
//!   disk image on macOS): USB hard drives and SSDs say they are fixed.
//!   Its disk is asked through a handle opened for no access, which needs
//!   no administrator, and closed at once.
//! - **Its identity:** FAT's and exFAT's serial as `XXXX-XXXX`, the one
//!   the volume keeps, as Linux spells it; any other's (NTFS, `ReFS`), the
//!   volume's GUID, which this machine gives it, lowercase. Its label, its
//!   volume's name.
//! - **Coming and going** ([`VolumesHeard`]): the volumes' device
//!   interface arriving or removed.
//! - **An eject asked** ([`RemovalHeard`]): a handle of its own on the
//!   volume, registered to be told when its removal is asked (from the
//!   taskbar's "Safely remove"); the engine lets go of everything held
//!   there, and the handle is closed before the call returns, so that the
//!   removal goes on. A volume a handle is open on is in use.

use std::ffi::c_void;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_NOTIFY_ACTION, CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL,
    CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL, CM_NOTIFY_ACTION_DEVICEQUERYREMOVE,
    CM_NOTIFY_EVENT_DATA, CM_NOTIFY_FILTER, CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE,
    CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE, CM_Register_Notification, CM_Unregister_Notification,
    CR_SUCCESS, HCMNOTIFICATION,
};
use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FindFirstVolumeW,
    FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{GUID_DEVINTERFACE_VOLUME, IOCTL_STORAGE_QUERY_PROPERTY};

use crate::removable::Removable;

/// `GetDriveTypeW`'s removable drive and optical disc.
const DRIVE_REMOVABLE: u32 = 2;
const DRIVE_CDROM: u32 = 5;
/// `STORAGE_BUS_TYPE`'s USB, SD, MMC and file-backed virtual disk.
const BUS_USB: u32 = 7;
const BUS_SD: u32 = 0xC;
const BUS_MMC: u32 = 0xD;
const BUS_VIRTUAL: u32 = 0xF;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn text(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    std::ffi::OsString::from_wide(&w[..end])
        .to_string_lossy()
        .into_owned()
}

/// The removable volumes mounted, each at each path it is mounted at.
pub fn mounted() -> Result<Vec<Removable>, String> {
    let mut name = [0u16; 64];
    // SAFETY: a buffer of its length, which the call fills.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), 64) };
    if find == INVALID_HANDLE_VALUE {
        return Err(format!(
            "the volumes cannot be listed: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut out = Vec::new();
    loop {
        let volume = text(&name);
        for at in points_of(&name).iter().filter(|at| resolves(at, &volume)) {
            if let Some(r) = removable_at(&volume, at) {
                out.push(r);
            }
        }
        // SAFETY: the search made above, and the buffer of its length.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), 64) } == 0 {
            break;
        }
    }
    // SAFETY: the search made above, closed once.
    unsafe { FindVolumeClose(find) };
    Ok(out)
}

/// The paths the volume named `name` (`\\?\Volume{…}\`, NUL-ended) is
/// mounted at.
fn points_of(name: &[u16]) -> Vec<String> {
    let mut buf = vec![0u16; 1024];
    let mut needed = 0u32;
    for _ in 0..2 {
        let len = u32::try_from(buf.len()).unwrap_or(u32::MAX);
        // SAFETY: a NUL-ended name, a buffer of `len` wide characters,
        // the count needed written where it is too small.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(name.as_ptr(), buf.as_mut_ptr(), len, &raw mut needed)
        } != 0;
        if ok {
            return buf
                .split(|&c| c == 0)
                .take_while(|p| !p.is_empty())
                .map(text)
                .collect();
        }
        buf.resize(needed as usize, 0);
    }
    Vec::new()
}

/// Whether `at` (`E:\`, `C:\mnt\stick\`) is where `volume` is mounted
/// now. The system's list of a volume's paths may keep a folder whose
/// mount point is gone or names another volume (the runner's virtual disk,
/// detached and attached again): a root there would be the folder, not
/// the volume, and answer the volume's index for it.
fn resolves(at: &str, volume: &str) -> bool {
    use windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW;
    let point = wide(at);
    let mut name = [0u16; 64];
    // SAFETY: a NUL-ended path, and a buffer of its length.
    let ok =
        unsafe { GetVolumeNameForVolumeMountPointW(point.as_ptr(), name.as_mut_ptr(), 64) } != 0;
    ok && text(&name).eq_ignore_ascii_case(volume)
}

/// The volume `volume` (`\\?\Volume{…}\`) at `at` (`E:\`), if it is
/// removable: its identity, filesystem and label.
fn removable_at(volume: &str, at: &str) -> Option<Removable> {
    let root = wide(at);
    // SAFETY: a NUL-ended path.
    let drive = unsafe { GetDriveTypeW(root.as_ptr()) };
    let plugged = drive == DRIVE_REMOVABLE
        || drive == DRIVE_CDROM
        || matches!(
            bus_of(volume),
            Some(BUS_USB | BUS_SD | BUS_MMC | BUS_VIRTUAL)
        );
    if !plugged {
        return None;
    }
    let mut label = [0u16; 261];
    let mut fs = [0u16; 261];
    let mut serial = 0u32;
    // SAFETY: a NUL-ended path, buffers of their lengths, the rest asked
    // of no one.
    let ok = unsafe {
        GetVolumeInformationW(
            root.as_ptr(),
            label.as_mut_ptr(),
            261,
            &raw mut serial,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            261,
        )
    } != 0;
    // No medium in its drive: nothing mounted.
    if !ok {
        return None;
    }
    let fstype = text(&fs);
    let id = if matches!(fstype.as_str(), "FAT" | "FAT32" | "exFAT") {
        Some(format!("{:04X}-{:04X}", serial >> 16, serial & 0xFFFF))
    } else {
        volume
            .split_once('{')
            .and_then(|(_, rest)| rest.split_once('}'))
            .map(|(guid, _)| guid.to_lowercase())
    };
    let label = text(&label);
    Some(Removable {
        id,
        // As clients name a folder: `C:\mnt\stick`, not `C:\mnt\stick\`; a
        // drive's root keeps its own (`E:\`).
        point: PathBuf::from(if at.len() > 3 {
            at.trim_end_matches('\\')
        } else {
            at
        }),
        fstype,
        label: (!label.is_empty()).then_some(label),
    })
}

/// The bus the disk of volume `volume` (`\\?\Volume{…}\`) is on, asked
/// through a handle opened for no access, closed at once.
fn bus_of(volume: &str) -> Option<u32> {
    let device = wide(volume.trim_end_matches('\\'));
    // SAFETY: a NUL-ended name, no access asked, everything shared: a
    // handle of ours, closed below.
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }
    // `STORAGE_PROPERTY_QUERY`: the device's descriptor
    // (`StorageDeviceProperty`), as it is (`PropertyStandardQuery`).
    let query = [0u32; 3];
    // `STORAGE_DEVICE_DESCRIPTOR`, its fixed part: its bus at byte 28.
    let mut descriptor = [0u32; 16];
    let mut got = 0u32;
    // SAFETY: the handle opened above; the query and the descriptor, of
    // their sizes, read and written for the call.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            query.as_ptr().cast(),
            12,
            descriptor.as_mut_ptr().cast(),
            64,
            &raw mut got,
            std::ptr::null_mut(),
        )
    } != 0;
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(handle) };
    (ok && got >= 32).then_some(descriptor[7])
}

/// Volumes arriving and removed heard, `on` called for each on a thread
/// of the system's, for as long as this is kept.
pub struct VolumesHeard {
    notify: HCMNOTIFICATION,
    _on: Box<Box<dyn Fn() + Send + Sync>>,
}

// SAFETY: a registration, ended once as it is dropped; its context is
// read by the system's threads alone, and is `Sync`.
unsafe impl Send for VolumesHeard {}
// SAFETY: as above.
unsafe impl Sync for VolumesHeard {}

unsafe extern "system" fn volumes_callback(
    _: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    if action == CM_NOTIFY_ACTION_DEVICEINTERFACEARRIVAL
        || action == CM_NOTIFY_ACTION_DEVICEINTERFACEREMOVAL
    {
        // SAFETY: the context given at registration, alive until it ends.
        let on = unsafe { &*context.cast::<Box<dyn Fn() + Send + Sync>>() };
        on();
    }
    0
}

impl VolumesHeard {
    /// Volumes heard from now on.
    pub fn new(on: Box<dyn Fn() + Send + Sync>) -> std::io::Result<VolumesHeard> {
        let on = Box::new(on);
        // SAFETY: an all-zero filter is a valid one, before its fields.
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = u32::try_from(std::mem::size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0);
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEINTERFACE;
        filter.u.DeviceInterface.ClassGuid = GUID_DEVINTERFACE_VOLUME;
        let mut notify: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: the filter, read for the call; the context, boxed, alive
        // as long as the registration; the registration written.
        let done = unsafe {
            CM_Register_Notification(
                &raw const filter,
                (&raw const *on).cast(),
                Some(volumes_callback),
                &raw mut notify,
            )
        };
        if done != CR_SUCCESS {
            return Err(std::io::Error::other(format!(
                "volumes cannot be heard: {done}"
            )));
        }
        Ok(VolumesHeard { notify, _on: on })
    }
}

impl Drop for VolumesHeard {
    fn drop(&mut self) {
        // SAFETY: the registration made, ended once, never from its own
        // callback.
        unsafe { CM_Unregister_Notification(self.notify) };
    }
}

struct Removal {
    volume: Mutex<Option<OwnedHandle>>,
    on: Box<dyn Fn() + Send + Sync>,
}

/// A volume's removal heard as it is asked: `on` called, which lets go of
/// everything held there, then the handle closed, before the call returns
/// and the removal goes on. Dropped off the thread that asks the removal
/// (ending a registration waits for its callback).
pub struct RemovalHeard {
    notify: HCMNOTIFICATION,
    removal: Box<Removal>,
}

// SAFETY: a registration, ended once as it is dropped; its context is a
// handle under a lock and a `Sync` closure.
unsafe impl Send for RemovalHeard {}
// SAFETY: as above.
unsafe impl Sync for RemovalHeard {}

unsafe extern "system" fn removal_callback(
    _: HCMNOTIFICATION,
    context: *const c_void,
    action: CM_NOTIFY_ACTION,
    _: *const CM_NOTIFY_EVENT_DATA,
    _: u32,
) -> u32 {
    if action == CM_NOTIFY_ACTION_DEVICEQUERYREMOVE {
        // SAFETY: the context given at registration, alive until it ends.
        let removal = unsafe { &*context.cast::<Removal>() };
        (removal.on)();
        removal
            .volume
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
    0
}

impl RemovalHeard {
    /// The removal of the volume holding `path` heard from now on, on a
    /// handle of its own.
    pub fn new(path: &Path, on: Box<dyn Fn() + Send + Sync>) -> std::io::Result<RemovalHeard> {
        let volume = crate::windows::journal::Volume::handle_of(path)?;
        let raw = volume.as_raw_handle();
        let removal = Box::new(Removal {
            volume: Mutex::new(Some(volume)),
            on,
        });
        // SAFETY: an all-zero filter is a valid one, before its fields.
        let mut filter: CM_NOTIFY_FILTER = unsafe { std::mem::zeroed() };
        filter.cbSize = u32::try_from(std::mem::size_of::<CM_NOTIFY_FILTER>()).unwrap_or(0);
        filter.FilterType = CM_NOTIFY_FILTER_TYPE_DEVICEHANDLE;
        filter.u.DeviceHandle.hTarget = raw;
        let mut notify: HCMNOTIFICATION = std::ptr::null_mut();
        // SAFETY: the filter, naming a handle of ours, read for the call;
        // the context, boxed, alive as long as the registration.
        let done = unsafe {
            CM_Register_Notification(
                &raw const filter,
                (&raw const *removal).cast(),
                Some(removal_callback),
                &raw mut notify,
            )
        };
        if done != CR_SUCCESS {
            return Err(std::io::Error::other(format!(
                "the volume's removal cannot be heard: {done}"
            )));
        }
        Ok(RemovalHeard { notify, removal })
    }
}

impl Drop for RemovalHeard {
    fn drop(&mut self) {
        // SAFETY: the registration made, ended once, never from its own
        // callback; its handle closed after, with `removal`.
        unsafe { CM_Unregister_Notification(self.notify) };
        self.removal
            .volume
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}
