//! What macOS says of a mounted volume's disk (`DiskArbitration`), to
//! tell one plugged in from one inside the machine
//! ([`crate::removable`]): whether its device is inside the machine,
//! whether its media is removable, its device's protocol (a disk image's
//! is `Disk Image`), and its volume's UUID and name.
//!
//! `DiskArbitration` is loaded on first use, not linked, as
//! `CoreServices` is (the watcher's): every `skwird` a client runs would
//! pay `CoreFoundation`'s initializer otherwise. A disk is described
//! synchronously, its session never scheduled: nothing is heard from it.

use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;

/// What a volume's disk is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Disk {
    /// Its device is inside the machine.
    pub internal: bool,
    /// Its media can be taken out of its device (an SD card).
    pub removable: bool,
    /// Its device's protocol: `USB`, `Secure Digital`, `Disk Image`,
    /// `PCI-Express`…
    pub protocol: Option<String>,
    /// Its volume's UUID, lowercase with its dashes: for FAT and exFAT,
    /// the one `DiskArbitration` derives from the serial its filesystem
    /// keeps.
    pub uuid: Option<String>,
    /// Its volume's name.
    pub name: Option<String>,
}

/// `kCFStringEncodingUTF8`.
const UTF8: u32 = 0x0800_0100;

struct Api {
    session: *mut c_void,
    from_bsd_name: unsafe extern "C" fn(*const c_void, *mut c_void, *const c_char) -> *mut c_void,
    copy_description: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    internal: *const c_void,
    removable: *const c_void,
    protocol: *const c_void,
    uuid: *const c_void,
    name: *const c_void,
    get_value: unsafe extern "C" fn(*const c_void, *const c_void) -> *const c_void,
    type_of: unsafe extern "C" fn(*const c_void) -> usize,
    boolean_type: usize,
    string_type: usize,
    uuid_type: usize,
    boolean_value: unsafe extern "C" fn(*const c_void) -> u8,
    uuid_string: unsafe extern "C" fn(*const c_void, *const c_void) -> *mut c_void,
    string_bytes: unsafe extern "C" fn(*const c_void, *mut c_char, isize, u32) -> u8,
    release: unsafe extern "C" fn(*const c_void),
}

// SAFETY: the session and the keys are never written after they are
// made, and `DiskArbitration`'s synchronous calls may be made from any
// thread.
unsafe impl Send for Api {}
// SAFETY: as above.
unsafe impl Sync for Api {}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(load).as_ref()
}

/// The symbol `name` of `image`, as type `T`: a function, or the address
/// of a constant.
///
/// # Safety
/// `T` is the symbol's type, as the system's headers declare it.
unsafe fn sym<T>(image: *mut c_void, name: &CStr) -> Option<T> {
    // SAFETY: a NUL-terminated name, of an image loaded.
    let p = unsafe { libc::dlsym(image, name.as_ptr()) };
    // SAFETY: a symbol found, of the caller's type, the size of a pointer.
    (!p.is_null()).then(|| unsafe { std::mem::transmute_copy::<*mut c_void, T>(&p) })
}

/// The value of the constant `name` of `image`: a `CFStringRef`.
///
/// # Safety
/// The symbol is a constant of a pointer's type.
unsafe fn key(image: *mut c_void, name: &CStr) -> Option<*const c_void> {
    // SAFETY: the caller's.
    let at: *const *const c_void = unsafe { sym(image, name)? };
    // SAFETY: the address of a constant, set as the image was loaded.
    let value = unsafe { *at };
    (!value.is_null()).then_some(value)
}

fn load() -> Option<Api> {
    let open = |path: &CStr| {
        // SAFETY: a NUL-terminated path; the image stays loaded, never
        // closed.
        let h = unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_LAZY) };
        (!h.is_null()).then_some(h)
    };
    let da = open(c"/System/Library/Frameworks/DiskArbitration.framework/DiskArbitration")?;
    let cf = open(c"/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;
    // SAFETY: each symbol is the function or constant of that name, of the
    // type its field says, as the system's headers declare it.
    unsafe {
        let session_create: unsafe extern "C" fn(*const c_void) -> *mut c_void =
            sym(da, c"DASessionCreate")?;
        let session = session_create(std::ptr::null());
        if session.is_null() {
            return None;
        }
        let type_id = |name: &CStr| -> Option<usize> {
            let f: unsafe extern "C" fn() -> usize = sym(cf, name)?;
            Some(f())
        };
        Some(Api {
            session,
            from_bsd_name: sym(da, c"DADiskCreateFromBSDName")?,
            copy_description: sym(da, c"DADiskCopyDescription")?,
            internal: key(da, c"kDADiskDescriptionDeviceInternalKey")?,
            removable: key(da, c"kDADiskDescriptionMediaRemovableKey")?,
            protocol: key(da, c"kDADiskDescriptionDeviceProtocolKey")?,
            uuid: key(da, c"kDADiskDescriptionVolumeUUIDKey")?,
            name: key(da, c"kDADiskDescriptionVolumeNameKey")?,
            get_value: sym(cf, c"CFDictionaryGetValue")?,
            type_of: sym(cf, c"CFGetTypeID")?,
            boolean_type: type_id(c"CFBooleanGetTypeID")?,
            string_type: type_id(c"CFStringGetTypeID")?,
            uuid_type: type_id(c"CFUUIDGetTypeID")?,
            boolean_value: sym(cf, c"CFBooleanGetValue")?,
            uuid_string: sym(cf, c"CFUUIDCreateString")?,
            string_bytes: sym(cf, c"CFStringGetCString")?,
            release: sym(cf, c"CFRelease")?,
        })
    }
}

impl Api {
    /// The value of `key` in `dict`, if it is of the type `of`.
    fn value(&self, dict: *const c_void, key: *const c_void, of: usize) -> Option<*const c_void> {
        // SAFETY: a dictionary of ours, alive for the call; a key of
        // `DiskArbitration`'s.
        let v = unsafe { (self.get_value)(dict, key) };
        // SAFETY: a value the dictionary holds, alive while it is.
        (!v.is_null() && unsafe { (self.type_of)(v) } == of).then_some(v)
    }

    fn boolean(&self, dict: *const c_void, key: *const c_void) -> Option<bool> {
        let v = self.value(dict, key, self.boolean_type)?;
        // SAFETY: a boolean, its type checked.
        Some(unsafe { (self.boolean_value)(v) } != 0)
    }

    /// A string's text, as UTF-8.
    fn text(&self, s: *const c_void) -> Option<String> {
        let mut buf = vec![0 as c_char; 1024];
        let len = isize::try_from(buf.len()).ok()?;
        // SAFETY: a string, and a buffer of `len` bytes, which the call
        // fills NUL-terminated or refuses.
        let ok = unsafe { (self.string_bytes)(s, buf.as_mut_ptr(), len, UTF8) } != 0;
        // SAFETY: NUL-terminated within the buffer, where the call did.
        ok.then(|| {
            unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned()
        })
    }

    fn string(&self, dict: *const c_void, key: *const c_void) -> Option<String> {
        self.text(self.value(dict, key, self.string_type)?)
    }

    fn uuid(&self, dict: *const c_void, key: *const c_void) -> Option<String> {
        let v = self.value(dict, key, self.uuid_type)?;
        // SAFETY: a UUID, its type checked: a new string, ours to release.
        let s = unsafe { (self.uuid_string)(std::ptr::null(), v) };
        if s.is_null() {
            return None;
        }
        let text = self.text(s);
        // SAFETY: the string made above, released once.
        unsafe { (self.release)(s) };
        text.map(|t| t.to_lowercase())
    }
}

/// What macOS says of the disk `bsd` (`disk4s1`), the device a volume is
/// mounted from; none where it says nothing.
pub fn describe(bsd: &str) -> Option<Disk> {
    let api = api()?;
    let name = CString::new(bsd).ok()?;
    // SAFETY: the session made once, never released; a NUL-terminated name:
    // a new disk, ours to release.
    let disk = unsafe { (api.from_bsd_name)(std::ptr::null(), api.session, name.as_ptr()) };
    if disk.is_null() {
        return None;
    }
    // SAFETY: the disk made above: a new dictionary, ours to release.
    let dict = unsafe { (api.copy_description)(disk) };
    let described = (!dict.is_null()).then(|| Disk {
        internal: api.boolean(dict, api.internal).unwrap_or(true),
        removable: api.boolean(dict, api.removable).unwrap_or(false),
        protocol: api.string(dict, api.protocol),
        uuid: api.uuid(dict, api.uuid),
        name: api.string(dict, api.name),
    });
    // SAFETY: what was made above, each released once.
    unsafe {
        if !dict.is_null() {
            (api.release)(dict);
        }
        (api.release)(disk);
    }
    described
}
