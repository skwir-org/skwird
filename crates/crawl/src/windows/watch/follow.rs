//! A root heard through its volume's change journal (`docs/PLAN.md`,
//! phase 4, step 7): the system service's way, for a volume whose journal
//! holds every change to it, in order, from any position it still keeps.
//!
//! - **From a position, not from a moment.** The journal's next record is
//!   taken when the root is followed ([`Follower::start`]); what is made
//!   after it is read whenever the thread reads, so nothing is lost before
//!   the thread's first read, as a directory watch's would be.
//! - **A read that waits:** `FSCTL_READ_USN_JOURNAL` asked for at least one
//!   byte, with no time limit, on a handle read asynchronously. The thread
//!   waits on it and on its stop event alone: idle is idle (rule 6).
//! - **Each record names its entry by its folder's identity and its own
//!   name.** The folder is opened by its identity through the root's
//!   folder, once a batch, and asked its path: an entry outside the root is
//!   not the root's. A folder gone since is not asked; its removal is a
//!   record in its own folder. A folder that cannot be opened for another
//!   reason is a doubt, and the root is checked (rule 1).
//! - **A cookie** made in the cookies' folder is said as one, in its turn.
//! - **What ends it:** a journal that dropped records past the position
//!   (`ERROR_JOURNAL_ENTRY_DELETED`) is an overflow, and reading goes on
//!   from its next record; a journal deleted, or made again, ends the
//!   watch ([`Heard::Lost`]).
//!
//! A volume with no journal is given one ([`JOURNAL_SIZE`]), as Everything
//! gives one: the journal is the volume's, kept by NTFS whatever reads it.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};

use windows_sys::Win32::Foundation::{
    ERROR_DELETE_PENDING, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING,
    ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, ERROR_PATH_NOT_FOUND, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_NAME_NORMALIZED,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFinalPathNameByHandleW, OPEN_EXISTING, VOLUME_NAME_DOS,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use super::{Heard, Queue, event};
use crate::windows::journal::{self, Position, Volume};
use crate::windows::security::open_by_id;
use crate::windows::{Handle, plain, verbatim};

/// The journal a volume is given where it has none: 32 MiB, grown and cut
/// by 8 MiB, as `fsutil` suggests for a volume of a few hundred thousand
/// files; NTFS drops its oldest records past it.
pub const JOURNAL_SIZE: (u64, u64) = (32 << 20, 8 << 20);

/// `USN_REASON_FILE_CREATE`: the record of an entry made.
const FILE_CREATE: u32 = 0x0000_0100;

/// A root's volume, its journal read from a position.
pub(super) struct Follower {
    /// The volume, for its journal's bounds.
    volume: Volume,
    /// The volume again, read asynchronously.
    reads: Handle,
    /// The root's folder, which folders are opened through by identity.
    hint: Handle,
    done: Handle,
    ov: Box<OVERLAPPED>,
    input: Box<READ_USN_JOURNAL_DATA_V0>,
    buffer: Vec<u64>,
    position: Position,
}

// SAFETY: moved once, to the thread that reads, and used by it alone
// after; the structures and the buffer stay at their places, boxed.
unsafe impl Send for Follower {}

impl Follower {
    /// Follow the journal of the volume holding the root at `root` from its
    /// next record, the journal made first where there is none.
    pub(super) fn start(root: &Path) -> std::io::Result<Follower> {
        let device = journal::device_of(root)?;
        let volume = Volume(journal::open_device(&device, 0)?);
        let found = match volume.journal() {
            Err(e) if e.raw_os_error() == i32::try_from(ERROR_JOURNAL_NOT_ACTIVE).ok() => {
                volume.create_journal(JOURNAL_SIZE.0, JOURNAL_SIZE.1)?;
                volume.journal()?
            }
            other => other?,
        };
        let reads = journal::open_device(&device, FILE_FLAG_OVERLAPPED)?;
        let name = verbatim(root);
        // SAFETY: a NUL-terminated name alive for the call, no security
        // attributes, no template; the handle given is ours to close.
        let hint = unsafe {
            CreateFileW(
                name.as_ptr(),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS,
                std::ptr::null_mut(),
            )
        };
        if hint == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Follower {
            volume,
            reads,
            hint: Handle(hint),
            done: event()?,
            // SAFETY: an all-zero structure is a valid one, before its event.
            ov: Box::new(unsafe { std::mem::zeroed() }),
            input: Box::default(),
            buffer: vec![0u64; 8 * 1024],
            position: Position {
                journal: found.id,
                next: found.next,
            },
        })
    }

    /// Ask for the records from the position on, the read waiting for one:
    /// whether it is pending or done.
    pub(super) fn ask(&mut self) -> bool {
        *self.input = READ_USN_JOURNAL_DATA_V0 {
            StartUsn: self.position.next,
            ReasonMask: u32::MAX,
            ReturnOnlyOnClose: 0,
            // No limit: the read waits until there is a record.
            Timeout: 0,
            BytesToWaitFor: 1,
            UsnJournalID: self.position.journal,
        };
        // SAFETY: an all-zero structure is a valid one, given its event.
        *self.ov = unsafe { std::mem::zeroed() };
        self.ov.hEvent = self.done.0;
        let len = u32::try_from(self.buffer.len() * 8).unwrap_or(u32::MAX);
        // SAFETY: the input and the buffer, at places of their own until
        // the call is done or its cancel has landed, which `run` waits for;
        // the structure likewise.
        let ok = unsafe {
            DeviceIoControl(
                self.reads.0,
                FSCTL_READ_USN_JOURNAL,
                std::ptr::from_ref(&*self.input).cast::<c_void>(),
                u32::try_from(size_of::<READ_USN_JOURNAL_DATA_V0>()).unwrap_or(u32::MAX),
                self.buffer.as_mut_ptr().cast::<c_void>(),
                len,
                std::ptr::null_mut(),
                &raw mut *self.ov,
            )
        };
        // SAFETY: no argument.
        ok != 0 || unsafe { GetLastError() } == ERROR_IO_PENDING
    }

    /// The path of the folder of identity `id`, opened through the root's
    /// folder: none if it is gone, an error if it cannot be opened.
    fn path_of(&self, id: u128) -> Result<Option<PathBuf>, u32> {
        let folder = match open_by_id(&self.hint, id, FILE_READ_ATTRIBUTES) {
            Ok(folder) => folder,
            Err(
                ERROR_INVALID_PARAMETER
                | ERROR_FILE_NOT_FOUND
                | ERROR_PATH_NOT_FOUND
                | ERROR_DELETE_PENDING,
            ) => return Ok(None),
            Err(other) => return Err(other),
        };
        let mut buffer = vec![0u16; 512];
        loop {
            // SAFETY: the buffer and its length in units; the count returned
            // says how much was written, or how much is needed.
            let written = unsafe {
                GetFinalPathNameByHandleW(
                    folder.0,
                    buffer.as_mut_ptr(),
                    u32::try_from(buffer.len()).unwrap_or(u32::MAX),
                    FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
                )
            } as usize;
            if written == 0 {
                // SAFETY: no argument.
                return Err(unsafe { GetLastError() });
            }
            if written < buffer.len() {
                buffer.truncate(written);
                return Ok(Some(plain(Path::new(&OsString::from_wide(&buffer)))));
            }
            buffer.resize(written + 1, 0);
        }
    }

    /// What a batch of records says of root `scope` at `root`: each entry
    /// once, in the order heard; a cookie made in `cookies` in its turn; an
    /// overflow at the end if a folder could not be told.
    fn heard(
        &self,
        records: &[journal::Record],
        scope: usize,
        root: &Path,
        cookies: Option<&Path>,
    ) -> Vec<Heard> {
        let mut folders: HashMap<u128, Option<PathBuf>> = HashMap::new();
        let mut said = HashSet::new();
        let mut out = Vec::new();
        let mut doubt = false;
        for r in records {
            let folder = if let Some(known) = folders.get(&r.parent) {
                known.clone()
            } else {
                let found = self.path_of(r.parent).unwrap_or_else(|_| {
                    doubt = true;
                    None
                });
                folders.insert(r.parent, found.clone());
                found
            };
            let Some(folder) = folder else { continue };
            let Ok(below) = folder.strip_prefix(root) else {
                continue;
            };
            let name = crate::wtf8::path(&r.name);
            let cookie = (r.reason & FILE_CREATE != 0 && cookies == Some(folder.as_path()))
                .then(|| name.to_str()?.strip_prefix("cookie-")?.parse::<u64>().ok())
                .flatten();
            if let Some(n) = cookie {
                out.push(Heard::Cookie(n));
                continue;
            }
            let rel = below.join(&name);
            if said.insert(rel.clone()) {
                out.push(Heard::Entry { scope, rel });
            }
        }
        if doubt {
            out.push(Heard::Overflow { scope });
        }
        out
    }
}

/// Read root `scope`'s journal, its first read asked, until `stop` is
/// set, each batch put in `queue`; each read counted in `reads`.
pub(super) fn run(
    mut f: Follower,
    stop: &Handle,
    scope: usize,
    root: &Path,
    queue: &Queue,
    cookies: &Mutex<Option<PathBuf>>,
    reads: &AtomicU64,
) {
    loop {
        let waits: [HANDLE; 2] = [stop.0, f.done.0];
        // SAFETY: two events of ours, alive for the wait.
        let woken = unsafe { WaitForMultipleObjects(2, waits.as_ptr(), 0, INFINITE) };
        let mut moved = 0u32;
        if woken == WAIT_OBJECT_0 {
            // SAFETY: the pending call's own structure, alive until its
            // cancel has landed, which the wait for its result waits for.
            unsafe {
                CancelIoEx(f.reads.0, &raw const *f.ov);
                GetOverlappedResult(f.reads.0, &raw const *f.ov, &raw mut moved, 1);
            }
            return;
        }
        reads.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the call's own structure, done.
        let ok = unsafe { GetOverlappedResult(f.reads.0, &raw const *f.ov, &raw mut moved, 1) };
        if ok == 0 {
            // SAFETY: no argument.
            if unsafe { GetLastError() } != ERROR_JOURNAL_ENTRY_DELETED {
                queue.push([Heard::Lost { scope }]);
                return;
            }
            // Records past the position dropped: read on from the next, the
            // root checked meanwhile, if the journal is still the same one.
            queue.push([Heard::Overflow { scope }]);
            match f.volume.journal() {
                Ok(j) if j.id == f.position.journal => f.position.next = j.next,
                _ => {
                    queue.push([Heard::Lost { scope }]);
                    return;
                }
            }
        } else {
            let bytes = &crate::windows::bytemuck(&f.buffer)[..moved as usize];
            let cookies = cookies
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            queue.push(f.heard(&journal::records(bytes), scope, root, cookies.as_deref()));
            if let Some(next) = journal::next_of(bytes) {
                f.position.next = next;
            }
        }
        if !f.ask() {
            queue.push([Heard::Lost { scope }]);
            return;
        }
    }
}

#[cfg(test)]
mod tests;
