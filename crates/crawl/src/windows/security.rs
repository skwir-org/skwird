//! What the system service judges an asker by on Windows (`docs/DESIGN.md`,
//! decision 4, "On Windows"): each folder's security id, which NTFS keeps
//! in its record, and the security descriptor each id names, fetched once
//! through a folder holding it. Both on the threads that read folders,
//! never on one that answers a client.

use windows_sys::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, GetLastError, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::{
    DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetKernelObjectSecurity,
    LABEL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
};
use windows_sys::Win32::Storage::FileSystem::{
    ExtendedFileIdType, FILE_FLAG_BACKUP_SEMANTICS, FILE_ID_128, FILE_ID_DESCRIPTOR,
    FILE_ID_DESCRIPTOR_0, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdType,
    OpenFileById, READ_CONTROL,
};
use windows_sys::Win32::System::Ioctl::{
    FSCTL_GET_NTFS_FILE_RECORD, NTFS_FILE_RECORD_INPUT_BUFFER,
};

use super::Handle;
use super::journal::control;
use super::mft;

/// The file of identity `id` on the volume `hint` is on (a handle to any
/// file there), opened with `access`, through no path: what it is now,
/// wherever it is. The error number where it cannot be.
pub(crate) fn open_by_id(hint: &Handle, id: u128, access: u32) -> Result<Handle, u32> {
    let mut descriptor = FILE_ID_DESCRIPTOR {
        dwSize: u32::try_from(size_of::<FILE_ID_DESCRIPTOR>()).unwrap_or(u32::MAX),
        Type: FileIdType,
        Anonymous: FILE_ID_DESCRIPTOR_0 { FileId: 0 },
    };
    if let Ok(narrow) = u64::try_from(id) {
        descriptor.Anonymous.FileId = i64::from_le_bytes(narrow.to_le_bytes());
    } else {
        descriptor.Type = ExtendedFileIdType;
        descriptor.Anonymous.ExtendedFileId = FILE_ID_128 {
            Identifier: id.to_le_bytes(),
        };
    }
    // SAFETY: a handle of ours, open; the descriptor alive for the call;
    // the handle given is ours to close.
    let file = unsafe {
        OpenFileById(
            hint.0,
            &raw const descriptor,
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            FILE_FLAG_BACKUP_SEMANTICS,
        )
    };
    if file == INVALID_HANDLE_VALUE {
        // SAFETY: no argument.
        return Err(unsafe { GetLastError() });
    }
    Ok(Handle(file))
}

/// The security descriptor of the folder `folder` (its file reference),
/// opened by its identity through `hint`: its owner, group, access list
/// and integrity label, self-relative, as `AccessCheck` takes it.
pub(crate) fn descriptor_of(hint: &Handle, folder: u64) -> std::io::Result<Vec<u8>> {
    let handle = open_by_id(hint, u128::from(folder), READ_CONTROL)
        .map_err(|n| std::io::Error::from_raw_os_error(i32::try_from(n).unwrap_or(i32::MAX)))?;
    let what = OWNER_SECURITY_INFORMATION
        | GROUP_SECURITY_INFORMATION
        | DACL_SECURITY_INFORMATION
        | LABEL_SECURITY_INFORMATION;
    let mut buffer = vec![0u8; 512];
    loop {
        let mut needed = 0u32;
        // SAFETY: the buffer and its length; the length needed is ours.
        let ok = unsafe {
            GetKernelObjectSecurity(
                handle.0,
                what,
                buffer.as_mut_ptr().cast(),
                u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                &raw mut needed,
            )
        };
        if ok != 0 {
            buffer.truncate(needed.max(1) as usize);
            return Ok(buffer);
        }
        // SAFETY: no argument.
        if unsafe { GetLastError() } != ERROR_INSUFFICIENT_BUFFER || needed as usize <= buffer.len()
        {
            return Err(std::io::Error::last_os_error());
        }
        buffer.resize(needed as usize, 0);
    }
}

/// The security id NTFS keeps in the record of `file` (its reference) on
/// the volume `volume` (a handle to the volume itself), as it holds it in
/// memory (`FSCTL_GET_NTFS_FILE_RECORD`): none if the record is another
/// file's now, or says none.
pub(crate) fn security_id(volume: &Handle, file: u64) -> std::io::Result<Option<u32>> {
    let input = NTFS_FILE_RECORD_INPUT_BUFFER {
        FileReferenceNumber: i64::from_le_bytes(mft::number(file).to_le_bytes()),
    };
    // Its reference and length, then a record of 4 KiB at most.
    let mut out = vec![0u64; (12 + 4096) / 8 + 1];
    let written = control(volume, FSCTL_GET_NTFS_FILE_RECORD, &input, &mut out)?;
    let bytes = &super::bytemuck(&out)[..written];
    let given = u64::from_le_bytes(
        bytes
            .get(..8)
            .and_then(|b| b.try_into().ok())
            .unwrap_or_default(),
    );
    let len = bytes
        .get(8..12)
        .and_then(|b| b.try_into().ok())
        .map_or(0, u32::from_le_bytes) as usize;
    // The record of another number, where the one asked is in no use.
    if mft::number(given) != mft::number(file) {
        return Ok(None);
    }
    let Some(record) = bytes.get(12..12 + len) else {
        return Ok(None);
    };
    match mft::decode_fixed(mft::number(file), record) {
        Ok(d) if d.sequence == mft::sequence(file) => Ok(d.standard.and_then(|s| s.security)),
        _ => Ok(None),
    }
}

/// Enable, in this process's token, the privilege to read any file
/// whatever its descriptor (`SeBackupPrivilege`), as the system service
/// runs with: a folder its descriptor denies the service is still opened
/// by its identity (`FILE_FLAG_BACKUP_SEMANTICS`), its descriptor read,
/// where it would be a doubt otherwise (`System Volume Information`,
/// which `LocalSystem` alone may open). Whether it is enabled now: an
/// elevated administrator's token and `LocalSystem`'s hold it, others not.
pub fn read_anything() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_NOT_ALL_ASSIGNED, HANDLE, LUID};
    use windows_sys::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    let name: Vec<u16> = "SeBackupPrivilege".encode_utf16().chain([0]).collect();
    let mut luid = LUID::default();
    // SAFETY: no system named (this one); a NUL-terminated name; the LUID
    // ours.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &raw mut luid) } == 0 {
        return false;
    }
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle; the token ours, closed below.
    if unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &raw mut token,
        )
    } == 0
    {
        return false;
    }
    let token = Handle(token);
    let wanted = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: the token, open to adjust; the one privilege the count says;
    // no previous state asked.
    let ok = unsafe {
        AdjustTokenPrivileges(
            token.0,
            0,
            &raw const wanted,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    // It succeeds without assigning a privilege the token does not hold.
    // SAFETY: no argument.
    ok != 0 && unsafe { GetLastError() } != ERROR_NOT_ALL_ASSIGNED
}
