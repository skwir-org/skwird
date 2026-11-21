//! Memory the allocator holds, freed but not given back (`docs/DESIGN.md`,
//! decision 10).
//!
//! An engine works in bursts (a first build, a refresh) and then idles,
//! for hours. glibc's allocator keeps what a burst freed for the bursts to
//! come, in three ways:
//! - small allocations, freed, stay in the arenas;
//! - its threshold for giving a large allocation a mapping of its own
//!   rises each time such a mapping is freed, up to 32 MB. The next large
//!   buffers then come from an arena's heap, and stay there once freed;
//! - `malloc_trim` gives back free pages inside an arena, but never
//!   shrinks the top of a thread's arena.
//!
//! So the engine fixes the threshold when it starts ([`settle`]). Every
//! allocation of 128 KiB or more gets a mapping of its own, given back
//! whole when freed, and a heap's free top is given back past 1 MiB. Then
//! it gives back what is free after a first build and after each refresh
//! ([`give_back`]). The packer (`skwird_index::packer`) and the check's
//! list of folders (`skwird_crawl::Stored`) make few small allocations,
//! in large buffers.
//!
//! This module speaks to the system's C library, so it allows unsafe code
//! (AGENTS.md, "Unsafe").

#![allow(unsafe_code)]

#[cfg(all(target_os = "linux", target_env = "gnu"))]
mod glibc {
    use std::ffi::c_int;

    // glibc's `malloc.h`.
    const M_TRIM_THRESHOLD: c_int = -1;
    const M_MMAP_THRESHOLD: c_int = -3;

    unsafe extern "C" {
        fn mallopt(param: c_int, value: c_int) -> c_int;
        fn malloc_trim(pad: usize) -> c_int;
    }

    pub fn settle() {
        // SAFETY: `mallopt` takes two integers and sets the allocator's
        // own parameters under its lock. Setting either parameter turns
        // off the threshold's rise; memory already allocated is not
        // touched.
        unsafe {
            mallopt(M_MMAP_THRESHOLD, 128 << 10);
            mallopt(M_TRIM_THRESHOLD, 1 << 20);
        }
    }

    pub fn give_back() {
        // SAFETY: `malloc_trim` takes no pointer. It walks glibc's own
        // arenas under their own locks, and returns their free pages to
        // the system; memory still allocated is not touched.
        unsafe {
            malloc_trim(0);
        }
    }
}

/// Fix the allocator's thresholds, once, as the engine starts.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn settle() {
    glibc::settle();
}

/// Give back to the system what the allocator holds freed.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn give_back() {
    glibc::give_back();
}

/// Elsewhere there are no such calls (musl's allocator gives freed pages
/// back as it goes): nothing is asked.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn settle() {}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn give_back() {}
