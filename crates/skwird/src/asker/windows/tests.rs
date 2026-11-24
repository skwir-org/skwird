use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;

use skwird_crawl::{Cancel, Exclusions, Options, Root, crawl_all};
use skwird_index::Sight;
use skwird_testkit::{Scratch, User};
use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Security::{DuplicateToken, SecurityIdentification};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use super::{Access, Token};

const TOKEN_QUERY_DUPLICATE: u32 = 0x0008 | 0x0002;

/// An identification token of `token`, as a pipe's server holds one.
fn identification(token: HANDLE) -> Token {
    let mut copy: HANDLE = std::ptr::null_mut();
    // SAFETY: a token of ours, open to be duplicated; the copy ours.
    assert_ne!(
        unsafe { DuplicateToken(token, SecurityIdentification, &raw mut copy) },
        0,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the copy the call made, ours alone.
    Token::of(unsafe { OwnedHandle::from_raw_handle(copy) }).unwrap()
}

/// Each folder judged as Windows judges its asker's own open: a user of
/// the test's own lists a folder its descriptor lets every user list, and
/// not one given to the administrators alone; an administrator lists both;
/// both pass through a folder they may not list, as every user may bypass
/// traverse checking. A folder whose descriptor was never read is not
/// known. On a test machine alone, which may make a user
/// (`skwird_testkit::throwaway`), on its NTFS volume.
#[test]
fn each_asker_is_judged_by_its_own_token() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "asker");
    s.dir(r"tree\open");
    skwird_testkit::administrators_alone(&s.dir(r"tree\closed"));
    let root = Root::open(&s.join("tree"), Exclusions::none()).unwrap();
    let opts = Options {
        threads: 2,
        acls: true,
        ..Options::default()
    };
    let crawl = crawl_all(&root, &opts, &Cancel::default()).unwrap();
    let owner = |rel: &str| {
        crawl
            .folders
            .iter()
            .find(|f| f.rel == Path::new(rel))
            .and_then(|f| f.stat)
            .map(|s| s.owner)
            .unwrap()
    };
    let dev = root.stat().id.dev;
    let user = User::make("skwird-test", "Sk-1wird.test!");
    let someone = identification(user.logon().as_raw_handle());
    let mut admin: HANDLE = std::ptr::null_mut();
    // SAFETY: this process's pseudo-handle; the token ours, closed below.
    assert_ne!(
        unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY_DUPLICATE, &raw mut admin) },
        0
    );
    let administrator = identification(admin);
    // SAFETY: the process's token, ours, closed once.
    drop(unsafe { OwnedHandle::from_raw_handle(admin) });
    assert!(!someone.is_system() && !administrator.is_system());
    assert_ne!(someone.sid, administrator.sid);
    let (open, closed) = (owner("open"), owner("closed"));
    assert_eq!(someone.may(dev, open, Access::List), Sight::Seen);
    assert_eq!(someone.may(dev, closed, Access::List), Sight::Hidden);
    assert_eq!(administrator.may(dev, open, Access::List), Sight::Seen);
    assert_eq!(administrator.may(dev, closed, Access::List), Sight::Seen);
    assert_eq!(
        someone.may(dev, closed, Access::Traverse),
        Sight::Seen,
        "traverse bypassed"
    );
    assert_eq!(
        someone.may(dev, 0, Access::List),
        Sight::Unknown,
        "an id never read"
    );
    assert_eq!(
        someone.may(dev, 0x7FFF_FFF0, Access::List),
        Sight::Unknown,
        "no descriptor held"
    );
    assert_eq!(
        someone.may(dev, skwird_crawl::OPEN_TO_ALL, Access::List),
        Sight::Seen
    );
}

/// `token`, its privilege to bypass traverse checking deleted
/// (`CreateRestrictedToken`): a token that passes through a folder only
/// where its descriptor lets it.
fn without_bypass(token: HANDLE) -> OwnedHandle {
    use windows_sys::Win32::Foundation::LUID;
    use windows_sys::Win32::Security::{
        CreateRestrictedToken, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW,
    };
    let name: Vec<u16> = "SeChangeNotifyPrivilege\0".encode_utf16().collect();
    let mut luid = LUID::default();
    // SAFETY: no system named; a NUL-terminated name; the LUID ours.
    assert_ne!(
        unsafe { LookupPrivilegeValueW(std::ptr::null(), name.as_ptr(), &raw mut luid) },
        0
    );
    let delete = [LUID_AND_ATTRIBUTES {
        Luid: luid,
        Attributes: 0,
    }];
    let mut restricted: HANDLE = std::ptr::null_mut();
    // SAFETY: a token of ours; no SID disabled or restricted; the one
    // privilege to delete; the new token ours.
    let ok = unsafe {
        CreateRestrictedToken(
            token,
            0,
            0,
            std::ptr::null(),
            1,
            delete.as_ptr(),
            0,
            std::ptr::null(),
            &raw mut restricted,
        )
    };
    assert_ne!(ok, 0, "{}", std::io::Error::last_os_error());
    // SAFETY: the token the call made, ours alone.
    unsafe { OwnedHandle::from_raw_handle(restricted) }
}

/// A folder above a root, judged by its descriptor
/// (`skwird_crawl::judged_by`): a token that may not bypass traverse
/// checking passes through one every user may pass through, not one the
/// administrators alone may; a token that may bypass passes through both,
/// unchecked. On a test machine alone, on its NTFS volume.
#[test]
fn a_folder_above_a_root_is_passed_through_as_its_descriptor_says() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    let s = Scratch::under(&volume, "above");
    let open = s.dir("open");
    let walled = s.dir("walled");
    skwird_testkit::administrators_alone(&walled);
    let user = User::make("skwird-above", "Sk-1wird.abv!");
    let logged_on = user.logon();
    let bypassing = identification(logged_on.as_raw_handle());
    let restricted = without_bypass(logged_on.as_raw_handle());
    let checked = identification(restricted.as_raw_handle());
    let (dev, open_id) = skwird_crawl::judged_by(&open).expect("judged by its descriptor");
    let (_, walled_id) = skwird_crawl::judged_by(&walled).expect("judged by its descriptor");
    assert_ne!(open_id, walled_id, "two descriptors");
    assert!(skwird_crawl::descriptor_on(dev, open_id).is_some(), "held");
    assert_eq!(checked.may(dev, open_id, Access::Traverse), Sight::Seen);
    assert_eq!(checked.may(dev, walled_id, Access::Traverse), Sight::Hidden);
    assert_eq!(bypassing.may(dev, walled_id, Access::Traverse), Sight::Seen);
}
