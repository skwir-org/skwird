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
//! - **A cookie** asked of the root ([`super::Watcher::cookie`]) is said
//!   once the journal is read past the record that was its next as the
//!   cookie was asked: every change made before was written before it.
//! - **What ends it:** a journal that dropped records past the position
//!   (`ERROR_JOURNAL_ENTRY_DELETED`) is an overflow, and reading goes on
//!   from its next record; a journal deleted, or made again, ends the
//!   watch ([`Heard::Lost`]). So does the root's folder no longer where it
//!   was ([`super::Place`]), asked once a batch renames or removes a
//!   folder: its records' folders' paths would never be below the root's
//!   again, and nothing heard.
//! - **A batch the follower cannot read whole** (a record cut short, of a
//!   version not known) is an overflow: the position moves past it.
//!
//! A volume with no journal is given one ([`JOURNAL_SIZE`]), as Everything
//! gives one: the journal is the volume's, kept by NTFS whatever reads it.

use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::Foundation::{
    ERROR_DELETE_PENDING, ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING,
    ERROR_JOURNAL_ENTRY_DELETED, ERROR_JOURNAL_NOT_ACTIVE, ERROR_PATH_NOT_FOUND, GetLastError,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_READ_ATTRIBUTES,
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED,
};
use windows_sys::Win32::System::Ioctl::{FSCTL_READ_USN_JOURNAL, READ_USN_JOURNAL_DATA_V0};
use windows_sys::Win32::System::Threading::{INFINITE, WaitForMultipleObjects};

use super::{Asked, Heard, Place, Queue, event, final_dos};
use crate::windows::journal::{self, Position, Volume};
use crate::windows::security::open_by_id;
use crate::windows::{Handle, verbatim};

/// `USN_REASON_*`: a name removed, a name given, a file deleted.
const RENAME_OLD_NAME: u32 = 0x0000_1000;
const RENAME_NEW_NAME: u32 = 0x0000_2000;
const FILE_DELETE: u32 = 0x0000_0200;

/// The journal a volume is given where it has none: 32 MiB, grown and cut
/// by 8 MiB, as `fsutil` suggests for a volume of a few hundred thousand
/// files; NTFS drops its oldest records past it.
pub const JOURNAL_SIZE: (u64, u64) = (32 << 20, 8 << 20);

/// A root's volume, its journal read from a position.
pub(super) struct Follower {
    /// The volume, for its journal's bounds.
    volume: Volume,
    /// The volume again, read asynchronously.
    reads: Handle,
    /// The root's folder, which folders are opened through by identity.
    hint: Handle,
    /// Where the root's folder was as it began to be followed.
    place: Place,
    /// Whether its volume is NTFS, whose first records are its metadata.
    ntfs: bool,
    done: Handle,
    ov: Box<OVERLAPPED>,
    input: Box<READ_USN_JOURNAL_DATA_V0>,
    buffer: Vec<u64>,
    /// Whether a read is asked and not yet done: cancelled, and waited
    /// for, as the follower is dropped.
    pending: bool,
    position: Position,
    /// Where the journal was as it started, while what came before it is
    /// replayed from a position kept: none once read past it, or where it
    /// started from the journal's next record.
    replay_until: Option<i64>,
    resumed: bool,
}

// SAFETY: moved once, to the thread that reads, and used by it alone
// after; the structures and the buffer stay at their places, boxed.
unsafe impl Send for Follower {}

impl Drop for Follower {
    fn drop(&mut self) {
        if self.pending {
            let mut moved = 0u32;
            // SAFETY: the pending call's own structure, input and buffer,
            // alive until its cancel has landed, which the wait for its
            // result waits for; the handles closed after, with the fields.
            unsafe {
                CancelIoEx(self.reads.0, &raw const *self.ov);
                GetOverlappedResult(self.reads.0, &raw const *self.ov, &raw mut moved, 1);
            }
        }
    }
}

impl Follower {
    /// Follow the journal of the volume holding the root at `root`, the
    /// journal made first where there is none: from `kept` where the
    /// journal still holds it (DESIGN 2, [`Position::holds`]), what changed
    /// since replayed; from its next record otherwise.
    pub(super) fn start(root: &Path, kept: Option<Position>) -> std::io::Result<Follower> {
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
        let hint = Handle(hint);
        Ok(Follower {
            volume,
            reads,
            place: Place::of(&hint),
            ntfs: crate::windows::filesystem(&hint) == "NTFS",
            hint,
            done: event()?,
            // SAFETY: an all-zero structure is a valid one, before its event.
            ov: Box::new(unsafe { std::mem::zeroed() }),
            input: Box::default(),
            buffer: vec![0u64; 8 * 1024],
            pending: false,
            position: kept.filter(|k| k.holds(&found)).unwrap_or(Position {
                journal: found.id,
                next: found.next,
            }),
            replay_until: kept
                .filter(|k| k.holds(&found) && k.next < found.next)
                .map(|_| found.next),
            resumed: kept.is_some_and(|k| k.holds(&found)),
        })
    }

    /// Where it reads from next.
    pub(super) fn position(&self) -> Position {
        self.position
    }

    /// Whether it started from the position kept; and whether what came
    /// before the journal's next record, as it started, is still to be
    /// read.
    pub(super) fn resumed(&self) -> (bool, bool) {
        (self.resumed, self.replay_until.is_some())
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
        self.pending = ok != 0 || unsafe { GetLastError() } == ERROR_IO_PENDING;
        self.pending
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
        final_dos(&folder).map(Some)
    }

    /// What a batch of records says of root `scope` at `root`, `whole`
    /// where every record of it was read: each entry once, in the order
    /// heard; an overflow at the end if a folder could not be told, or a
    /// record not read. And whether the root's folder is no longer where
    /// it was, asked where a folder was renamed or removed: its watch then
    /// ends, its path leading to nothing heard.
    fn heard(
        &self,
        records: &[journal::Record],
        whole: bool,
        scope: usize,
        root: &Path,
    ) -> Vec<Heard> {
        let mut folders: HashMap<u128, Option<PathBuf>> = HashMap::new();
        let mut said = HashSet::new();
        let mut out = Vec::new();
        let mut doubt = !whole;
        let moved = records.iter().any(|r| {
            r.is_folder() && r.reason & (RENAME_OLD_NAME | RENAME_NEW_NAME | FILE_DELETE) != 0
        }) && self.place.moved(&self.hint);
        for r in records {
            // The volume's metadata (`$Extend` and the like), below no root:
            // never opened, as a folder of it is no one's to open.
            if self.ntfs && is_metadata(r.parent) {
                continue;
            }
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
            let rel = below.join(crate::wtf8::path(&r.name));
            if said.insert(rel.clone()) {
                out.push(Heard::Entry { scope, rel });
            }
        }
        if doubt {
            out.push(Heard::Overflow { scope });
        }
        // Last, where the follower looks for it.
        if moved {
            out.push(Heard::Lost { scope });
        }
        out
    }
}

/// Whether the file `id` names is an NTFS volume's metadata: one of the
/// first sixteen records but the root folder's (`$MFT`, `$Extend`…), which
/// no root holds. Asked of NTFS's references alone: `ReFS`'s 128-bit ids
/// follow no such rule.
fn is_metadata(id: u128) -> bool {
    let number = id & 0x0000_FFFF_FFFF_FFFF;
    number < 16 && number != 5
}

/// The cookies held until the journal is read up to the record each
/// waits for, said once it is: in their turn, after what was read before.
fn due(held: &mut Vec<(i64, u64)>, next: i64) -> Vec<Heard> {
    let (due, waiting): (Vec<_>, Vec<_>) = held.drain(..).partition(|&(at, _)| next >= at);
    *held = waiting;
    due.into_iter().map(|(_, n)| Heard::Cookie(n)).collect()
}

/// Read root `scope`'s journal, its first read asked, until `stop` is
/// set, each batch put in `queue`; each read counted in `reads`; each
/// cookie `asked` said once the journal is read up to the record that was
/// its next as it was asked.
pub(super) fn run(
    mut f: Follower,
    stop: &Handle,
    asked: &Asked,
    scope: usize,
    root: &Path,
    queue: &Queue,
    reads: &AtomicU64,
) {
    let mut held: Vec<(i64, u64)> = Vec::new();
    loop {
        let waits: [HANDLE; 3] = [stop.0, f.done.0, asked.event.0];
        // SAFETY: three events of ours, alive for the wait.
        let woken = unsafe { WaitForMultipleObjects(3, waits.as_ptr(), 0, INFINITE) };
        if woken == WAIT_OBJECT_0 + 2 {
            let numbers = asked.take();
            // A journal that cannot be asked, or made again: the cookies
            // are never said, and their syncs check once they are late.
            if let Ok(j) = f.volume.journal()
                && j.id == f.position.journal
            {
                held.extend(numbers.into_iter().map(|n| (j.next, n)));
            }
            queue.push(due(&mut held, f.position.next));
            continue;
        }
        if woken == WAIT_OBJECT_0 {
            // The read pending is cancelled as the follower is dropped.
            return;
        }
        reads.fetch_add(1, Ordering::Relaxed);
        let mut moved = 0u32;
        // SAFETY: the call's own structure, done.
        let ok = unsafe { GetOverlappedResult(f.reads.0, &raw const *f.ov, &raw mut moved, 1) };
        f.pending = false;
        if ok == 0 {
            // SAFETY: no argument.
            if unsafe { GetLastError() } != ERROR_JOURNAL_ENTRY_DELETED {
                queue.push([Heard::Lost { scope }]);
                return;
            }
            // Records past the position dropped: read on from the next, the
            // root checked meanwhile, if the journal is still the same one.
            queue.push([Heard::Overflow { scope }]);
            // The replay is cut short: the check makes the root current.
            f.replay_until = None;
            match f.volume.journal() {
                Ok(j) if j.id == f.position.journal => f.position.next = j.next,
                _ => {
                    queue.push([Heard::Lost { scope }]);
                    return;
                }
            }
            queue.push(due(&mut held, f.position.next));
        } else {
            let bytes = &crate::windows::bytemuck(&f.buffer)[..moved as usize];
            let (records, whole) = journal::records_whole(bytes);
            let mut said = f.heard(&records, whole, scope, root);
            if said.last() == Some(&Heard::Lost { scope }) {
                queue.push(said);
                return;
            }
            if let Some(next) = journal::next_of(bytes) {
                f.position.next = next;
            }
            said.extend(due(&mut held, f.position.next));
            // After what it read, in the same turn: once that is applied, the
            // position holds it.
            said.push(Heard::Position {
                scope,
                at: f.position,
            });
            if f.replay_until.is_some_and(|until| f.position.next >= until) {
                f.replay_until = None;
                said.push(Heard::Replayed { scope });
            }
            queue.push(said);
        }
        if !f.ask() {
            queue.push([Heard::Lost { scope }]);
            return;
        }
    }
}

#[cfg(test)]
mod tests;
