//! Reads at the lowest priority the system's I/O has
//! ([`crate::Options::background`]): a removable volume is read at its
//! pace, the copy the user plugged it in for first (`docs/DESIGN.md`,
//! Protocol, "Removable volumes").
//!
//! - Linux: the idle class (`ioprio_set`), which reads only when no one
//!   else does. A thread made after inherits it.
//! - macOS: the throttled policy (`setiopolicy_np`), the thread's alone.
//! - Windows: the background mode (`THREAD_MODE_BACKGROUND_BEGIN`), which
//!   lowers the thread's I/O and memory priority, the thread's alone.
//!
//! Each thread that reads asks it for itself: the crawl's do as they
//! start.

// The systems' calls, which neither rustix nor std binds; each block says
// why it is sound.
#![allow(unsafe_code)]

/// This thread's reads given the lowest priority the system's I/O has.
/// A system that refuses leaves them as they were: read sooner, never
/// wrong.
#[cfg(target_os = "linux")]
pub fn background() {
    // `IOPRIO_WHO_PROCESS` with 0 names the calling thread; the idle
    // class, `IOPRIO_CLASS_IDLE` shifted by `IOPRIO_CLASS_SHIFT`.
    const WHO_PROCESS: libc::c_long = 1;
    const IDLE: libc::c_long = 3 << 13;
    // SAFETY: a call of plain integers, about this thread alone.
    unsafe {
        libc::syscall(libc::SYS_ioprio_set, WHO_PROCESS, 0 as libc::c_long, IDLE);
    }
}

#[cfg(target_os = "macos")]
pub fn background() {
    use std::ffi::c_int;
    // `sys/resource.h`.
    const IOPOL_TYPE_DISK: c_int = 0;
    const IOPOL_SCOPE_THREAD: c_int = 1;
    const IOPOL_THROTTLE: c_int = 3;
    unsafe extern "C" {
        fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
    }
    // SAFETY: a call of plain integers, about this thread alone.
    unsafe {
        setiopolicy_np(IOPOL_TYPE_DISK, IOPOL_SCOPE_THREAD, IOPOL_THROTTLE);
    }
}

#[cfg(windows)]
pub fn background() {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };
    // SAFETY: the calling thread's own pseudo-handle, and a mode.
    unsafe {
        SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN);
    }
}

/// Elsewhere, no such priority: read as any read is.
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub fn background() {}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// The thread's class is idle once asked, and a thread made after
    /// inherits it.
    #[test]
    fn a_thread_reads_in_the_idle_class_once_asked() {
        // SAFETY: a call of plain integers, about this thread alone.
        let class = || unsafe { libc::syscall(libc::SYS_ioprio_get, 1 as libc::c_long, 0 as libc::c_long) } >> 13;
        std::thread::spawn(move || {
            assert_ne!(class(), 3);
            super::background();
            assert_eq!(class(), 3);
            std::thread::spawn(move || assert_eq!(class(), 3))
                .join()
                .unwrap();
        })
        .join()
        .unwrap();
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::ffi::c_int;

    unsafe extern "C" {
        fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
    }

    /// The thread's disk policy is throttled once asked, and another
    /// thread's is not: each thread that reads asks for itself.
    #[test]
    fn a_thread_reads_throttled_once_asked() {
        // `IOPOL_TYPE_DISK`, `IOPOL_SCOPE_THREAD`, `IOPOL_THROTTLE`.
        // SAFETY: a call of plain integers, about this thread alone.
        let policy = || unsafe { getiopolicy_np(0, 1) };
        std::thread::spawn(move || {
            assert_ne!(policy(), 3);
            super::background();
            assert_eq!(policy(), 3);
            std::thread::spawn(move || assert_ne!(policy(), 3))
                .join()
                .unwrap();
        })
        .join()
        .unwrap();
    }
}

#[cfg(all(test, windows))]
mod tests {
    use windows_sys::Win32::Foundation::{ERROR_THREAD_MODE_ALREADY_BACKGROUND, GetLastError};
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_MODE_BACKGROUND_BEGIN,
    };

    /// The thread is in the background mode once asked: asked again, the
    /// system says it is already. Another thread is not. Wine's answer is
    /// not Windows'.
    #[test]
    fn a_thread_reads_in_the_background_once_asked() {
        if skwird_testkit::under_wine() {
            return;
        }
        // Whether this thread is in the background mode already.
        let already = || {
            // SAFETY: the calling thread's own pseudo-handle, and a mode.
            let begun =
                unsafe { SetThreadPriority(GetCurrentThread(), THREAD_MODE_BACKGROUND_BEGIN) };
            // SAFETY: no argument.
            begun == 0 && unsafe { GetLastError() } == ERROR_THREAD_MODE_ALREADY_BACKGROUND
        };
        std::thread::spawn(move || {
            super::background();
            assert!(already(), "not in the background mode");
            std::thread::spawn(move || assert!(!already(), "inherited"))
                .join()
                .unwrap();
        })
        .join()
        .unwrap();
    }
}
