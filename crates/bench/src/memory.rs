//! What a process holds of memory, as its system counts it
//! (`skwird-bench memory PID`): resident, and its own, what no file
//! backs. The second is what an engine's budget for memory at rest holds
//! to (DESIGN 10, "Memory, in two"): what is resident besides is its
//! index's files mapped and the system's libraries, the page cache's to
//! drop. A process of the same user is asked; nothing is read of what it
//! holds.
//!
//! - Linux: its status (`VmRSS`, `RssAnon`).
//! - macOS: `proc_pid_rusage` (`ri_resident_size`, `ri_phys_footprint`,
//!   what macOS counts against it, compressed pages included). The
//!   resident size `ps` says counts what its allocator has given back as
//!   reusable, and the system's libraries it touched.
//! - Windows: `GetProcessMemoryInfo` (its working set, and its private
//!   bytes).

// macOS' and Windows' calls, which no crate the workspace takes binds;
// each block says why it is sound.
#![allow(unsafe_code)]

/// What a process holds, in bytes.
pub struct Held {
    pub resident: u64,
    pub own: u64,
}

#[cfg(target_os = "linux")]
pub fn of(pid: u32) -> Result<Held, String> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status"))
        .map_err(|e| format!("no process {pid}: {e}"))?;
    let kib = |key: &str| {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().strip_suffix("kB"))
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map(|v| v * 1024)
            .ok_or_else(|| format!("process {pid}'s status says no {key}"))
    };
    Ok(Held {
        resident: kib("VmRSS:")?,
        own: kib("RssAnon:")?,
    })
}

#[cfg(target_os = "macos")]
pub fn of(pid: u32) -> Result<Held, String> {
    use std::ffi::{c_int, c_void};

    /// `RUSAGE_INFO_V0` of `sys/resource.h`: the fields before and up to
    /// the footprint, the first version's, which every later one begins
    /// with.
    #[repr(C)]
    #[derive(Default)]
    struct Info {
        uuid: [u8; 16],
        user_time: u64,
        system_time: u64,
        pkg_idle_wkups: u64,
        interrupt_wkups: u64,
        pageins: u64,
        wired_size: u64,
        resident_size: u64,
        phys_footprint: u64,
        proc_start_abstime: u64,
        proc_exit_abstime: u64,
    }
    const RUSAGE_INFO_V0: c_int = 0;
    unsafe extern "C" {
        // libproc's, in libSystem.
        fn proc_pid_rusage(pid: c_int, flavor: c_int, buffer: *mut c_void) -> c_int;
    }
    let pid = c_int::try_from(pid).map_err(|_| format!("no process {pid}"))?;
    let mut info = Info::default();
    // SAFETY: the first version's structure, whole and ours for the call,
    // which writes that version's fields alone.
    let done = unsafe { proc_pid_rusage(pid, RUSAGE_INFO_V0, (&raw mut info).cast()) };
    if done != 0 {
        return Err(format!(
            "process {pid} cannot be asked: {}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(Held {
        resident: info.resident_size,
        own: info.phys_footprint,
    })
}

#[cfg(windows)]
pub fn of(pid: u32) -> Result<Held, String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

    // SAFETY: no handle inherited; the handle given is ours to close.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(format!(
            "process {pid} cannot be asked: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: an all-zero structure is a valid one.
    let mut counters: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    let size = u32::try_from(std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>()).unwrap_or(0);
    counters.cb = size;
    // SAFETY: a handle of ours open for the call, and the extended
    // structure, its size given, which begins as the plain one.
    let done = unsafe {
        GetProcessMemoryInfo(
            process,
            (&raw mut counters).cast::<PROCESS_MEMORY_COUNTERS>(),
            size,
        )
    };
    let error = std::io::Error::last_os_error();
    // SAFETY: the handle opened above, closed once.
    unsafe { CloseHandle(process) };
    if done == 0 {
        return Err(format!("process {pid} cannot be asked: {error}"));
    }
    Ok(Held {
        resident: u64::try_from(counters.WorkingSetSize).unwrap_or(u64::MAX),
        own: u64::try_from(counters.PrivateUsage).unwrap_or(u64::MAX),
    })
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn of(_: u32) -> Result<Held, String> {
    Err("a process's memory is asked on Linux, macOS and Windows".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This process holds memory of its own, and more resident: what it
    /// allocates shows in its own.
    #[test]
    fn a_process_says_what_it_holds() {
        let before = of(std::process::id()).unwrap();
        // Wine answers the call, with no counts in it: proven on Windows.
        #[cfg(windows)]
        if skwird_testkit::under_wine() {
            return;
        }
        assert!(before.own > 0 && before.resident > 0);
        // Touched, so that it is resident, and held while asked.
        let held = vec![1u8; 64 << 20];
        let after = of(std::process::id()).unwrap();
        assert!(
            after.own >= before.own + (48 << 20),
            "{} then {}",
            before.own,
            after.own
        );
        drop(held);
    }
}
