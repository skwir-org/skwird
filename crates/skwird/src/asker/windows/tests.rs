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
