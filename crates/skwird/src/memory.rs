//! Memory the allocator holds, freed but not given back (`docs/DESIGN.md`,
//! decision 10).
//!
//! A first build frees much of what it held once its index is saved and
//! mapped from its files. glibc's allocator keeps freed small allocations
//! for the process, and an engine lives on, idle. The packer
//! (`skwird_index::packer`) makes few small allocations; what is left is
//! given back here, once. On a tree of a million entries, it took the
//! engine from 32 MB after its build to 18 MB, an index read back's.
//!
//! This module speaks to the system's C library, so it allows unsafe code
//! (AGENTS.md, "Unsafe").

#![allow(unsafe_code)]

/// Give back to the system what the allocator holds freed.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
pub fn give_back() {
    unsafe extern "C" {
        fn malloc_trim(pad: usize) -> std::ffi::c_int;
    }
    // SAFETY: `malloc_trim` takes no pointer. It walks glibc's own arenas
    // under their own locks, and returns their free pages to the system;
    // memory still allocated is not touched.
    unsafe {
        malloc_trim(0);
    }
}

/// Elsewhere there is no such call (musl's allocator gives freed pages
/// back as it goes): nothing is asked.
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
pub fn give_back() {}
