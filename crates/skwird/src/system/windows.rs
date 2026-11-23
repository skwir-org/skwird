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
//!   (DESIGN 5), and their own files of exclusions read: taken only if
//!   none but they, `LocalSystem` and the administrators may write them,
//!   as OpenSSH for Windows takes a user's keys. A folder a user keeps
//!   out is theirs if its owner is them.

#![allow(unsafe_code)] // The registry and security descriptors have no safe binding; each block says why it is sound.

use std::collections::BTreeMap;
use std::ffi::{OsString, c_void};
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

use super::Own;
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

/// Who besides a user may write a file of theirs the service takes:
/// `LocalSystem`, the administrators, and the placeholders that stand for
/// its owner (`CREATOR OWNER`, `OWNER RIGHTS`), whose owner is checked.
const MAY_WRITE: &[&str] = &[SYSTEM, ADMINISTRATORS, "S-1-3-0", "S-1-3-4"];
/// `BUILTIN\Administrators`.
const ADMINISTRATORS: &str = "S-1-5-32-544";

/// The rights that change a file, its access or its owner, or remove it.
const WRITES: u32 = 0x0002 // FILE_WRITE_DATA
    | 0x0004 // FILE_APPEND_DATA
    | 0x0010 // FILE_WRITE_EA
    | 0x0001_0000 // DELETE
    | 0x0004_0000 // WRITE_DAC
    | 0x0008_0000 // WRITE_OWNER
    | 0x1000_0000 // GENERIC_ALL
    | 0x4000_0000; // GENERIC_WRITE

/// An entry that allows (`ACCESS_ALLOWED_ACE_TYPE`), and those that deny,
/// of an access list's.
const ALLOWED: u8 = 0;
const DENIED: &[u8] = &[1, 6, 10, 12];

/// A file of a user's own, at `rel` in their home, read for the system
/// service: opened beneath the home through no link
/// (`skwird_crawl::open_own`), and taken only if its owner is the user,
/// `LocalSystem` or the administrators, and none but them may write it,
/// as OpenSSH for Windows takes a user's keys. At most a mebibyte.
pub fn read_own(home: &Path, uid: u32, rel: &str) -> Own {
    use std::io::Read;
    let file = match skwird_crawl::open_own(home, Path::new(rel)) {
        skwird_crawl::OwnFile::Absent => return Own::Absent,
        skwird_crawl::OwnFile::Refused => return Own::Refused,
        skwird_crawl::OwnFile::Opened(file) => file,
    };
    let Some(sid) = sid_of(uid) else {
        return Own::Refused;
    };
    if !theirs_alone(&file, &sid) {
        return Own::Refused;
    }
    let mut bytes = Vec::new();
    match file.take(1 << 20).read_to_end(&mut bytes) {
        Ok(_) => Own::Read(bytes),
        Err(_) => Own::Refused,
    }
}

/// Whether user `uid`, of home `home`, owns the folder an exclusion of
/// theirs resolved to: at `resolved`, of identity `id` if one is there.
/// `LocalSystem` owns every folder. A folder there is theirs if it is
/// still that folder and its owner is them; a path where none is there
/// yet, if it is in their home and they own that.
pub fn owned(uid: u32, home: &Path, resolved: &Path, id: Option<skwird_crawl::FileId>) -> bool {
    if uid == 0 {
        return true;
    }
    let Some(sid) = sid_of(uid) else {
        return false;
    };
    let owned_by = |path: &Path, id: Option<skwird_crawl::FileId>| {
        skwird_crawl::open_folder(path).is_ok_and(|(file, at)| {
            id.is_none_or(|id| id == at) && owner_of(&file).is_some_and(|o| o == sid)
        })
    };
    match id {
        Some(id) => owned_by(resolved, Some(id)),
        None => resolved.starts_with(home) && owned_by(home, None),
    }
}

/// The security of the file open as `file`: its owner's SID, and whether
/// none but `sid`, `LocalSystem` and the administrators may write it.
fn security_of(file: &std::fs::File, sid: Option<&str>) -> Option<(String, bool)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION, GetAce, INHERIT_ONLY_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    };
    let mut owner: PSID = std::ptr::null_mut();
    let mut dacl: *mut ACL = std::ptr::null_mut();
    let mut held: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    let what = if sid.is_some() {
        OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION
    } else {
        OWNER_SECURITY_INFORMATION
    };
    // SAFETY: the file's handle, opened to read its security; the owner
    // and the list point into the descriptor given, which is ours to
    // free, and freed after they are read.
    let failed = unsafe {
        GetSecurityInfo(
            file.as_raw_handle(),
            SE_FILE_OBJECT,
            what,
            &raw mut owner,
            std::ptr::null_mut(),
            &raw mut dacl,
            std::ptr::null_mut(),
            &raw mut held,
        )
    };
    if failed != 0 {
        return None;
    }
    let held = crate::service::Descriptor(held);
    let owner = crate::service::sid_text(owner)?;
    let Some(sid) = sid else {
        drop(held);
        return Some((owner, false));
    };
    let may = |s: &str| s == sid || MAY_WRITE.contains(&s);
    // A list that is not there grants everyone everything.
    let mut alone = may(&owner) && !dacl.is_null();
    // SAFETY: the list, in the descriptor held, its header read.
    let count = if alone {
        unsafe { (*dacl).AceCount }
    } else {
        0
    };
    for i in 0..u32::from(count) {
        let mut ace: *mut c_void = std::ptr::null_mut();
        // SAFETY: the list, an entry within its count; the pointer given
        // points into the list.
        if unsafe { GetAce(dacl, i, &raw mut ace) } == 0 {
            alone = false;
            break;
        }
        // SAFETY: every entry begins with its header.
        let header = unsafe { &*ace.cast::<ACE_HEADER>() };
        if u32::from(header.AceFlags) & INHERIT_ONLY_ACE != 0 || DENIED.contains(&header.AceType) {
            continue;
        }
        if header.AceType != ALLOWED {
            // An entry that allows by other means (an object's, a
            // condition's): not read, so taken as one that may write.
            alone = false;
            break;
        }
        // SAFETY: an entry that allows, as its type says; its SID starts
        // where its last field does, within its size.
        let allowed = unsafe { &*ace.cast::<ACCESS_ALLOWED_ACE>() };
        if allowed.Mask & WRITES == 0 {
            continue;
        }
        let who = crate::service::sid_text((&raw const allowed.SidStart).cast_mut().cast());
        if !who.is_some_and(|w| may(&w)) {
            alone = false;
            break;
        }
    }
    drop(held);
    Some((owner, alone))
}

fn owner_of(file: &std::fs::File) -> Option<String> {
    security_of(file, None).map(|(owner, _)| owner)
}

fn theirs_alone(file: &std::fs::File, sid: &str) -> bool {
    security_of(file, Some(sid)).is_some_and(|(owner, alone)| {
        (owner == sid || owner == SYSTEM || owner == ADMINISTRATORS) && alone
    })
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
