//! The volumes of this machine and where each is mounted: what the system
//! service's roots are chosen from on Windows (`docs/DESIGN.md`, decision
//! 1), as Linux's are from its mount table. Each volume is named by its
//! GUID path, mounted at a drive's letter, a folder, both, or nowhere.

use std::ffi::OsString;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::PathBuf;

use windows_sys::Win32::Foundation::{
    ERROR_MORE_DATA, ERROR_NO_MORE_FILES, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    FindFirstVolumeW, FindNextVolumeW, FindVolumeClose, GetDriveTypeW, GetVolumeInformationW,
    GetVolumePathNamesForVolumeNameW, OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::{
    IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_PROPERTY_QUERY,
    StorageDeviceProperty,
};

use super::Handle;
use super::journal::control;

/// A drive of the machine's own (`GetDriveTypeW`): not removable, not a
/// share, not an optical disc, not memory.
const DRIVE_FIXED: u32 = 3;

/// A volume of this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mounted {
    /// Its GUID path (`\\?\Volume{…}\`).
    pub name: OsString,
    /// Where it is mounted: a drive's root (`C:\`), a folder.
    pub paths: Vec<PathBuf>,
    /// Its filesystem's name (`NTFS`, `FAT32`, `exFAT`, `ReFS`), empty
    /// where it has none to say (a reader with no card in it).
    pub fs: String,
    /// A drive of the machine's own, as Windows says (`DRIVE_FIXED`).
    pub fixed: bool,
    /// On a bus a drive is taken away by (USB, SD, MMC), or of a medium
    /// that is: a disk on USB is fixed, as Windows says, but taken away.
    pub removable: bool,
}

/// Every volume of this machine, in the order Windows gives them.
pub fn mounted() -> std::io::Result<Vec<Mounted>> {
    // A GUID path is 49 units with its NUL.
    let mut name = [0u16; 64];
    // SAFETY: the buffer and its length in units; the handle given is
    // closed below, once the search ends.
    let find = unsafe { FindFirstVolumeW(name.as_mut_ptr(), 64) };
    if find == INVALID_HANDLE_VALUE {
        return Err(std::io::Error::last_os_error());
    }
    let mut out = Vec::new();
    let ended = loop {
        let len = name.iter().position(|&u| u == 0).unwrap_or(name.len());
        out.push(describe(&name[..len]));
        // SAFETY: the search's handle, open; the buffer and its length.
        if unsafe { FindNextVolumeW(find, name.as_mut_ptr(), 64) } == 0 {
            // SAFETY: no argument.
            break unsafe { GetLastError() };
        }
    };
    // SAFETY: the search's handle, closed once.
    unsafe { FindVolumeClose(find) };
    if ended != ERROR_NO_MORE_FILES {
        return Err(std::io::Error::from_raw_os_error(
            i32::try_from(ended).unwrap_or(i32::MAX),
        ));
    }
    Ok(out)
}

/// The volume of GUID path `name` (with its last `\`, no NUL).
fn describe(name: &[u16]) -> Mounted {
    let z: Vec<u16> = name.iter().copied().chain([0]).collect();
    let mut fs = [0u16; 64];
    // SAFETY: a NUL-terminated root; the name's buffer and its length in
    // units; no other field asked.
    let ok = unsafe {
        GetVolumeInformationW(
            z.as_ptr(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            fs.as_mut_ptr(),
            64,
        )
    };
    let fs = if ok == 0 {
        String::new()
    } else {
        let end = fs.iter().position(|&u| u == 0).unwrap_or(fs.len());
        String::from_utf16_lossy(&fs[..end])
    };
    // SAFETY: a NUL-terminated root.
    let fixed = unsafe { GetDriveTypeW(z.as_ptr()) } == DRIVE_FIXED;
    Mounted {
        name: OsString::from_wide(name),
        paths: paths_of(&z),
        fs,
        fixed,
        removable: taken_away(name),
    }
}

/// Where the volume `z` (its GUID path, NUL-terminated) is mounted.
fn paths_of(z: &[u16]) -> Vec<PathBuf> {
    let mut buffer = vec![0u16; 256];
    loop {
        let mut needed = 0u32;
        // SAFETY: a NUL-terminated name; the buffer and its length in
        // units; the length needed ours.
        let ok = unsafe {
            GetVolumePathNamesForVolumeNameW(
                z.as_ptr(),
                buffer.as_mut_ptr(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &raw mut needed,
            )
        };
        if ok != 0 {
            break;
        }
        // SAFETY: no argument.
        if unsafe { GetLastError() } != ERROR_MORE_DATA || needed as usize <= buffer.len() {
            return Vec::new();
        }
        buffer.resize(needed as usize, 0);
    }
    // Each path NUL-terminated, an empty one last.
    buffer
        .split(|&u| u == 0)
        .take_while(|p| !p.is_empty())
        .map(|p| PathBuf::from(OsString::from_wide(p)))
        .collect()
}

/// Whether the volume `name` (its GUID path) is on a bus a drive is taken
/// away by, or of a medium that is. Its device is asked with no access to
/// it, which any user may; one that does not answer is not said to be.
fn taken_away(name: &[u16]) -> bool {
    // Without its last `\`, the name opens the volume, not its root.
    let mut device: Vec<u16> = name.to_vec();
    if device.last() == Some(&u16::from(b'\\')) {
        device.pop();
    }
    device.push(0);
    // SAFETY: a NUL-terminated name alive for the call, no access asked,
    // no security attributes, no template; the handle given is ours to
    // close.
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return false;
    }
    let handle = Handle(handle);
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut out = [0u64; 128];
    let Ok(written) = control(&handle, IOCTL_STORAGE_QUERY_PROPERTY, &query, &mut out) else {
        return false;
    };
    // STORAGE_DEVICE_DESCRIPTOR, read by its fields' offsets: whether its
    // medium is removable (a byte at 10), its bus (32 bits at 28).
    let bytes = &super::bytemuck(&out)[..written.min(out.len() * 8)];
    let Some(bus) = bytes.get(28..32).and_then(|b| b.try_into().ok()) else {
        return false;
    };
    let removable_medium = bytes.get(10).is_some_and(|&b| b != 0);
    let bus = i32::from_le_bytes(bus);
    removable_medium || [BusTypeUsb, BusTypeSd, BusTypeMmc].contains(&bus)
}

/// The GUID path of the volume mounted at `path` (a drive's root, or a
/// folder a volume is mounted at), for a test to find it in
/// [`mounted`].
pub fn name_at(path: &std::path::Path) -> std::io::Result<OsString> {
    let z: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut name = [0u16; 64];
    // SAFETY: a NUL-terminated mount point; the buffer and its length.
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetVolumeNameForVolumeMountPointW(
            z.as_ptr(),
            name.as_mut_ptr(),
            64,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let end = name.iter().position(|&u| u == 0).unwrap_or(name.len());
    Ok(OsString::from_wide(&name[..end]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests' own NTFS volume (on a runner, `S:\`) is listed: mounted
    /// at its letter, of NTFS, a drive of the machine's own, not taken
    /// away, as a virtual disk the machine attached is.
    #[test]
    fn the_tests_volume_is_listed_where_it_is_mounted() {
        let Some(root) = skwird_testkit::ntfs() else {
            return;
        };
        let name = name_at(&root).unwrap();
        let all = mounted().unwrap();
        let volume = all
            .iter()
            .find(|v| v.name == name)
            .unwrap_or_else(|| panic!("{name:?} not in {all:?}"));
        assert!(volume.paths.contains(&root), "{volume:?}");
        assert_eq!(volume.fs, "NTFS");
        assert!(volume.fixed, "{volume:?}");
        assert!(!volume.removable, "{volume:?}");
        assert!(
            all.iter()
                .all(|v| v.name.to_string_lossy().starts_with(r"\\?\Volume{")),
            "{all:?}"
        );
    }
}
