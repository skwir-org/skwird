//! What the engine asks of its system beside its folders and its
//! transport: the lock that keeps one engine per scope (`docs/DESIGN.md`,
//! decision 1), and a thread's processor time, which its share of a core
//! is counted in (DESIGN 8).

#![cfg_attr(windows, allow(unsafe_code))] // Windows' calls have no safe binding; each block says why it is sound.

use std::path::Path;
use std::time::Duration;

/// The lock at `path`, taken: held until the file is dropped. A second
/// engine waits here until the first lets it go, even during its first
/// crawl, and takes over then; it does not poll.
pub fn lock(path: &Path) -> Result<std::fs::File, String> {
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("cannot open the lock: {e}"))?;
    take(&lock).map_err(|e| format!("cannot take the lock: {e}"))?;
    Ok(lock)
}

#[cfg(unix)]
fn take(lock: &std::fs::File) -> std::io::Result<()> {
    rustix::fs::flock(lock, rustix::fs::FlockOperation::LockExclusive).map_err(Into::into)
}

#[cfg(windows)]
fn take(lock: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LockFileEx};
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: an all-zero structure locks from the file's start.
    let mut at: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the file's handle, open for the call; the whole file's range,
    // exclusively, waited for (no flag to fail at once); the structure is
    // alive for the call, which returns once the lock is held.
    let ok = unsafe {
        LockFileEx(
            lock.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &raw mut at,
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// The processor time this thread has taken.
#[cfg(unix)]
pub fn thread_time() -> Duration {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
    Duration::new(
        u64::try_from(t.tv_sec).unwrap_or(0),
        u32::try_from(t.tv_nsec).unwrap_or(0),
    )
}

/// The processor time this thread has taken, the system's and its own.
#[cfg(windows)]
pub fn thread_time() -> Duration {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: the pseudo-handle of this thread, never closed; four
    // structures alive for the call.
    let ok = unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &raw mut created,
            &raw mut exited,
            &raw mut kernel,
            &raw mut user,
        )
    };
    if ok == 0 {
        return Duration::ZERO;
    }
    // In 100 ns.
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    let total = ticks(kernel) + ticks(user);
    Duration::new(total / 10_000_000, (total % 10_000_000) as u32 * 100)
}

/// Make the file at `path`, appended to, this process's standard error,
/// where the engine writes its log: a client starts the engine with none
/// of its own handles on Windows, so it names the log instead
/// (`SKWIRD_LOG`).
#[cfg(windows)]
pub fn log_to(path: &Path) -> std::io::Result<()> {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{STD_ERROR_HANDLE, SetStdHandle};
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    // Kept open for the process's life: the standard error from now on.
    let handle = file.into_raw_handle();
    // SAFETY: a handle of this process's, open, never closed.
    if unsafe { SetStdHandle(STD_ERROR_HANDLE, handle) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
