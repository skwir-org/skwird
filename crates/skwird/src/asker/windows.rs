//! Who asks the system service on Windows, and what they may see
//! (`docs/DESIGN.md`, decision 4, "On Windows"): the token of the pipe's
//! client, taken when it connects (`skwird_client::pipe::Stream::
//! client_token`), and each folder judged with `AccessCheck` by the
//! security descriptor its security id names, as the system would judge
//! the asker's own open of it.
//!
//! Judgements are kept for the connection, by volume, id and access: a
//! descriptor is shared by every folder of the same access, so a million
//! folders take as many checks as they have descriptors. A folder whose
//! descriptor is not held yet is not known, and that is never kept: it is
//! judged once its descriptor is.
#![allow(unsafe_code)] // Tokens and access checks have no safe binding; each block says why it is sound.

use std::collections::HashMap;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::sync::{Mutex, PoisonError};

use skwird_index::Sight;
use windows_sys::Win32::Foundation::LUID;
use windows_sys::Win32::Security::{
    AccessCheck, GENERIC_MAPPING, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, PRIVILEGE_SET,
    PrivilegeCheck,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ALL_ACCESS, FILE_GENERIC_EXECUTE, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
    FILE_LIST_DIRECTORY, FILE_TRAVERSE,
};

/// `LocalSystem`, which the system service runs as.
const SYSTEM: &str = "S-1-5-18";
/// `PRIVILEGE_SET_ALL_NECESSARY`: every privilege named is needed.
const ALL_NECESSARY: u32 = 1;

/// What an asker may do in a folder: list it, or pass through it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Access {
    List,
    Traverse,
}

/// An asker's token, and what it was judged to see.
pub struct Token {
    handle: OwnedHandle,
    /// Its user's SID, as text.
    pub sid: String,
    /// Whether it may pass through a folder it may not list, unchecked
    /// (`SeChangeNotifyPrivilege`, every user's by default), as Windows
    /// lets it.
    bypass: bool,
    judged: Mutex<HashMap<(u64, u32, Access), Sight>>,
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token")
            .field("sid", &self.sid)
            .finish_non_exhaustive()
    }
}

impl PartialEq for Token {
    fn eq(&self, other: &Token) -> bool {
        self.sid == other.sid
    }
}

impl Eq for Token {}

impl Token {
    /// The asker `handle` is the token of: an impersonation token, opened
    /// to be queried, as a pipe's server takes it.
    pub fn of(handle: OwnedHandle) -> std::io::Result<Token> {
        let sid = skwird_client::pipe::token_sid(&handle)?;
        let bypass = holds(&handle, "SeChangeNotifyPrivilege")?;
        Ok(Token {
            handle,
            sid,
            bypass,
            judged: Mutex::new(HashMap::new()),
        })
    }

    /// `LocalSystem`: the service itself, which may see everything.
    pub fn is_system(&self) -> bool {
        self.sid == SYSTEM
    }

    /// What this asker may do in a folder on the volume `dev` whose owner,
    /// as the system service reads it, is `owner`: a security id, whose
    /// descriptor is judged; [`skwird_crawl::OPEN_TO_ALL`] on a volume with
    /// no security, which anyone may list; 0, not read, not known.
    pub fn may(&self, dev: u64, owner: u32, access: Access) -> Sight {
        if access == Access::Traverse && self.bypass {
            return Sight::Seen;
        }
        match owner {
            0 => return Sight::Unknown,
            skwird_crawl::OPEN_TO_ALL => return Sight::Seen,
            _ => {}
        }
        let key = (dev, owner, access);
        if let Some(&known) = self
            .judged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&key)
        {
            return known;
        }
        let Some(descriptor) = skwird_crawl::descriptor_on(dev, owner) else {
            return Sight::Unknown;
        };
        let Some(sight) = self.check(&descriptor, access) else {
            return Sight::Unknown;
        };
        self.judged
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, sight);
        sight
    }

    /// Whether `descriptor` grants this token `access`: none if it cannot
    /// be judged.
    fn check(&self, descriptor: &[u8], access: Access) -> Option<Sight> {
        // A descriptor's fields are 32-bit: copied where they are aligned.
        let mut aligned = vec![0u64; descriptor.len().div_ceil(8)];
        // SAFETY: as many bytes as the descriptor holds, into a buffer at
        // least as long; neither overlaps the other.
        unsafe {
            std::ptr::copy_nonoverlapping(
                descriptor.as_ptr(),
                aligned.as_mut_ptr().cast::<u8>(),
                descriptor.len(),
            );
        }
        let mapping = GENERIC_MAPPING {
            GenericRead: FILE_GENERIC_READ,
            GenericWrite: FILE_GENERIC_WRITE,
            GenericExecute: FILE_GENERIC_EXECUTE,
            GenericAll: FILE_ALL_ACCESS,
        };
        let desired = match access {
            Access::List => FILE_LIST_DIRECTORY,
            Access::Traverse => FILE_TRAVERSE,
        };
        let mut privileges = vec![0u64; 32];
        let mut privileges_len = u32::try_from(privileges.len() * 8).unwrap_or(u32::MAX);
        let mut granted = 0u32;
        let mut status = 0;
        // SAFETY: the descriptor, self-relative and aligned, read alone;
        // the token, open to be queried; the mapping; the privileges'
        // buffer and its length; the results ours.
        let ok = unsafe {
            AccessCheck(
                aligned.as_mut_ptr().cast(),
                self.handle.as_raw_handle(),
                desired,
                &raw const mapping,
                privileges.as_mut_ptr().cast(),
                &raw mut privileges_len,
                &raw mut granted,
                &raw mut status,
            )
        };
        if ok == 0 {
            return None;
        }
        Some(if status != 0 && granted & desired == desired {
            Sight::Seen
        } else {
            Sight::Hidden
        })
    }
}

/// Whether `token` holds the privilege `name`, enabled.
fn holds(token: &OwnedHandle, name: &str) -> std::io::Result<bool> {
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    let mut luid = LUID::default();
    // SAFETY: no system named (this one); a NUL-terminated name; the LUID
    // ours.
    if unsafe { LookupPrivilegeValueW(std::ptr::null(), wide.as_ptr(), &raw mut luid) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut set = PRIVILEGE_SET {
        PrivilegeCount: 1,
        Control: ALL_NECESSARY,
        Privilege: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: 0,
        }],
    };
    let mut result = 0;
    // SAFETY: the token, open to be queried; the set, of the one privilege
    // its count says; the result ours.
    if unsafe { PrivilegeCheck(token.as_raw_handle(), &raw mut set, &raw mut result) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(result != 0)
}

#[cfg(test)]
mod tests;
