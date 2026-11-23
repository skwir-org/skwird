//! The system service's roots and users on Windows (`docs/DESIGN.md`,
//! decisions 1, 4 and 5; `docs/PLAN.md`, phase 4, step 7).
//!
//! - **Its roots:** every volume of the machine's own that stores files
//!   (NTFS, FAT, exFAT), at a place it is mounted, its drive's letter
//!   first; never one on a bus a drive is taken away by (open question
//!   8), a share, an optical disc or memory. `ReFS` is not a root yet
//!   (DESIGN 4).
//! - **Its users** are told by their SIDs. Each is given a number, as Unix
//!   numbers its users, kept in the service's state (`users`), so that
//!   what a user registered is still theirs after a restart.
//!   `LocalSystem` is 0, as root is.
//! - **Their homes** are their profiles, as the machine lists them
//!   (`ProfileList`), where the default private folders are left out
//!   (DESIGN 5).

#![allow(unsafe_code)] // The registry has no safe binding; each block says why it is sound.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use skwird_crawl::volumes::Mounted;
use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
use windows_sys::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_NOEXPAND, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ,
    RegCloseKey, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};

use crate::engine::log;

/// The filesystems that store files on Windows, as volumes name them.
const STORING: &[&str] = &["NTFS", "FAT", "FAT32", "exFAT"];
/// `LocalSystem`.
const SYSTEM: &str = "S-1-5-18";
/// The number the first user other than `LocalSystem` is given, past the
/// ones Unix keeps for its system's.
const FIRST: u32 = 1000;
/// Where the machine lists its users' profiles.
const PROFILES: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList";

/// The system service's roots: [`roots_of`] the machine's volumes.
pub fn roots() -> Vec<PathBuf> {
    match skwird_crawl::volumes::mounted() {
        Ok(all) => roots_of(&all),
        Err(e) => {
            log(&format!("cannot list the machine's volumes: {e}"));
            Vec::new()
        }
    }
}

/// Of `volumes`, each that is a drive of the machine's own, never taken
/// away, of a filesystem that stores files, at a place it is mounted: its
/// drive's root where it has one, its shortest place otherwise. A volume
/// mounted nowhere is not reached.
pub fn roots_of(volumes: &[Mounted]) -> Vec<PathBuf> {
    let is_drive = |p: &PathBuf| p.as_os_str().len() == 3;
    let mut roots: Vec<PathBuf> = volumes
        .iter()
        .filter(|v| v.fixed && !v.removable && STORING.contains(&v.fs.as_str()))
        .filter_map(|v| {
            v.paths
                .iter()
                .find(|p| is_drive(p))
                .or_else(|| v.paths.iter().min_by_key(|p| p.as_os_str().len()))
                .cloned()
        })
        .collect();
    roots.sort();
    roots.dedup();
    roots
}

/// Each SID's number, and the file they are kept in.
struct Numbers {
    file: PathBuf,
    by_sid: BTreeMap<String, u32>,
}

static NUMBERS: Mutex<Option<Numbers>> = Mutex::new(None);

/// Keep the numbers given to users in `state` (its file `users`), those
/// it holds read first. The system service's alone: a user's engine
/// numbers no one.
pub fn keep_numbers(state: &Path) {
    let file = state.join("users");
    let by_sid = std::fs::read(&file)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    *NUMBERS.lock().unwrap_or_else(PoisonError::into_inner) = Some(Numbers { file, by_sid });
}

/// The number of the user `sid` names: `LocalSystem`'s 0; each other's
/// given the first time they are met, and kept. None where no numbers are
/// kept ([`keep_numbers`]), or this one cannot be.
pub fn number_of(sid: &str) -> Option<u32> {
    if sid == SYSTEM {
        return Some(0);
    }
    let mut held = NUMBERS.lock().unwrap_or_else(PoisonError::into_inner);
    let numbers = held.as_mut()?;
    if let Some(&n) = numbers.by_sid.get(sid) {
        return Some(n);
    }
    let n = numbers
        .by_sid
        .values()
        .max()
        .map_or(Some(FIRST), |m| m.checked_add(1))?;
    numbers.by_sid.insert(sid.to_owned(), n);
    // Kept before the number is given: a number not kept could be given
    // another user after a restart, with what this one registered.
    let tmp = numbers.file.with_extension("tmp");
    let kept = serde_json::to_vec(&numbers.by_sid).is_ok_and(|bytes| {
        std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &numbers.file).is_ok()
    });
    if !kept {
        numbers.by_sid.remove(sid);
        log("cannot keep the number given a user: they are refused");
        return None;
    }
    Some(n)
}

/// Whether users are numbered here: the system service's, once it keeps
/// their numbers ([`keep_numbers`]).
pub fn numbering() -> bool {
    NUMBERS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .is_some()
}

/// The SID of the user numbered `number`, as [`number_of`] gave it.
pub fn sid_of(number: u32) -> Option<String> {
    if number == 0 {
        return Some(SYSTEM.to_owned());
    }
    let held = NUMBERS.lock().unwrap_or_else(PoisonError::into_inner);
    held.as_ref()?
        .by_sid
        .iter()
        .find(|&(_, &n)| n == number)
        .map(|(sid, _)| sid.clone())
}

/// The users the system service keeps exclusions for (DESIGN 5): each
/// profile's user, numbered, and their home.
pub fn users() -> Vec<(u32, PathBuf)> {
    profiles()
        .into_iter()
        .filter_map(|(sid, home)| Some((number_of(&sid)?, home)))
        .collect()
}

/// A key of the registry's, closed once dropped.
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: a key the registry opened, closed once.
        unsafe { RegCloseKey(self.0) };
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain([0]).collect()
}

/// Whether a profile's key names a user's: a local or a domain account's
/// (`S-1-5-21-…`), or a cloud directory's (`S-1-12-1-…`). Not the
/// system's own services', nor a profile set aside (`….bak`).
fn is_users(sid: &str) -> bool {
    (sid.starts_with("S-1-5-21-") || sid.starts_with("S-1-12-1-"))
        && sid
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'S')
}

/// Each user's SID and home, as the machine lists their profiles.
fn profiles() -> Vec<(String, PathBuf)> {
    let path = wide(PROFILES);
    let mut key: HKEY = std::ptr::null_mut();
    // SAFETY: a predefined key, a NUL-terminated path; the key given is
    // ours to close.
    if unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, path.as_ptr(), 0, KEY_READ, &raw mut key) }
        != ERROR_SUCCESS
    {
        log("cannot read the users' profiles: no home's private folders are known");
        return Vec::new();
    }
    let key = Key(key);
    let mut out = Vec::new();
    for index in 0.. {
        // A key's name is at most 255 units.
        let mut name = [0u16; 256];
        let mut len = 256u32;
        // SAFETY: the key, open; the name's buffer and its length in
        // units; no class or time asked.
        let got = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                name.as_mut_ptr(),
                &raw mut len,
                std::ptr::null(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if got == ERROR_NO_MORE_ITEMS {
            break;
        }
        if got != ERROR_SUCCESS {
            continue;
        }
        let sid = String::from_utf16_lossy(&name[..len as usize]);
        if !is_users(&sid) {
            continue;
        }
        if let Some(home) = home_of(&key, &name[..=len as usize]) {
            out.push((sid, home));
        }
    }
    out
}

/// The home of the profile whose key, below `key`, is `sid` (its name,
/// NUL-terminated), its environment's names expanded.
fn home_of(key: &Key, sid: &[u16]) -> Option<PathBuf> {
    let value = wide("ProfileImagePath");
    let mut data = vec![0u16; 260];
    loop {
        let mut bytes = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
        // SAFETY: the key, open; NUL-terminated names; the buffer and its
        // length in bytes. A string as it is kept, its names not expanded.
        let got = unsafe {
            RegGetValueW(
                key.0,
                sid.as_ptr(),
                value.as_ptr(),
                RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_NOEXPAND,
                std::ptr::null_mut(),
                data.as_mut_ptr().cast(),
                &raw mut bytes,
            )
        };
        if got == ERROR_MORE_DATA {
            data.resize((bytes as usize).div_ceil(2) + 1, 0);
            continue;
        }
        if got != ERROR_SUCCESS {
            return None;
        }
        break;
    }
    let end = data.iter().position(|&u| u == 0).unwrap_or(data.len());
    let kept: Vec<u16> = data[..end].iter().copied().chain([0]).collect();
    let mut expanded = vec![0u16; 520];
    loop {
        // SAFETY: a NUL-terminated string; the buffer and its length in
        // units.
        let needed = unsafe {
            ExpandEnvironmentStringsW(
                kept.as_ptr(),
                expanded.as_mut_ptr(),
                u32::try_from(expanded.len()).unwrap_or(u32::MAX),
            )
        } as usize;
        if needed == 0 {
            return None;
        }
        if needed <= expanded.len() {
            expanded.truncate(needed - 1);
            break;
        }
        expanded.resize(needed, 0);
    }
    let home = PathBuf::from(OsString::from_wide(&expanded));
    home.is_absolute().then_some(home)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(paths: &[&str], fs: &str, fixed: bool, removable: bool) -> Mounted {
        Mounted {
            name: OsString::from(r"\\?\Volume{0}\"),
            paths: paths.iter().map(PathBuf::from).collect(),
            fs: fs.to_owned(),
            fixed,
            removable,
        }
    }

    /// The volumes of the machine's own that store files are roots, at
    /// their drive's letter where they have one; a volume taken away, of
    /// `ReFS`, of no place, or of a disc is not.
    #[test]
    fn the_roots_are_the_machines_own_volumes_that_store_files() {
        let all = [
            volume(&[r"C:\"], "NTFS", true, false),
            volume(&[r"C:\mnt\data\", r"D:\"], "NTFS", true, false),
            volume(
                &[r"C:\mnt\deep\folder\", r"C:\mnt\x\"],
                "exFAT",
                true,
                false,
            ),
            volume(&[r"E:\"], "FAT32", true, true),
            volume(&[r"F:\"], "ReFS", true, false),
            volume(&[], "NTFS", true, false),
            volume(&[r"G:\"], "UDF", false, true),
            volume(&[r"H:\"], "FAT", true, false),
        ];
        assert_eq!(
            roots_of(&all),
            [r"C:\", r"C:\mnt\x\", r"D:\", r"H:\"]
                .map(PathBuf::from)
                .to_vec()
        );
    }

    /// A user's profile is a local, domain or cloud account's; the
    /// system's services' and one set aside are not.
    #[test]
    fn a_profile_is_a_users_when_its_sid_is_an_accounts() {
        assert!(is_users("S-1-5-21-1-2-3-1001"));
        assert!(is_users("S-1-12-1-1-2-3-4"));
        assert!(!is_users("S-1-5-18"));
        assert!(!is_users("S-1-5-19"));
        assert!(!is_users("S-1-5-21-1-2-3-1001.bak"));
    }

    /// `LocalSystem` is 0; each other user the next number from 1000,
    /// kept, and read back as given.
    #[test]
    fn each_user_is_numbered_once_and_kept() {
        let s = skwird_testkit::Scratch::new("win-numbers");
        keep_numbers(s.path());
        assert_eq!(number_of(SYSTEM), Some(0));
        let a = number_of("S-1-5-21-1-2-3-1001");
        let b = number_of("S-1-5-21-1-2-3-1002");
        assert_eq!((a, b), (Some(FIRST), Some(FIRST + 1)));
        assert_eq!(number_of("S-1-5-21-1-2-3-1001"), a);
        keep_numbers(s.path());
        assert_eq!(number_of("S-1-5-21-1-2-3-1002"), b, "read back as kept");
        assert_eq!(sid_of(FIRST + 1).as_deref(), Some("S-1-5-21-1-2-3-1002"));
        assert_eq!(sid_of(0).as_deref(), Some(SYSTEM));
        *NUMBERS.lock().unwrap() = None;
        assert_eq!(
            number_of("S-1-5-21-1-2-3-1003"),
            None,
            "none kept, none given"
        );
    }
}
