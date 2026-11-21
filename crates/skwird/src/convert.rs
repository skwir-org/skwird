//! The index's terms in the protocol's, and back.

use std::path::Path;

use skwird_crawl::{Attrs, Entry, FileId, Kind, Time, Why};
use skwird_index::{Hit, Proof};
use skwird_protocol as wire;

pub fn time(t: Time) -> wire::Instant {
    wire::Instant {
        sec: t.sec,
        nsec: t.nsec,
    }
}

pub fn time_of(t: wire::Instant) -> Time {
    Time {
        sec: t.sec,
        nsec: t.nsec,
    }
}

pub fn kind(k: Kind) -> wire::Kind {
    match k {
        Kind::File => wire::Kind::File,
        Kind::Folder => wire::Kind::Folder,
        Kind::Link => wire::Kind::Link,
        Kind::Other => wire::Kind::Other,
    }
}

pub fn kind_of(k: wire::Kind) -> Kind {
    match k {
        wire::Kind::File => Kind::File,
        wire::Kind::Folder => Kind::Folder,
        wire::Kind::Link => Kind::Link,
        wire::Kind::Other => Kind::Other,
    }
}

pub fn why(w: Why) -> wire::Why {
    match w {
        Why::Excluded => wire::Why::Excluded,
        Why::Private => wire::Why::Private,
        Why::Denied => wire::Why::Denied,
        Why::Failed(e) => wire::Why::Failed(e),
        Why::CutShort(e) => wire::Why::CutShort(e),
        Why::Gone => wire::Why::Gone,
        Why::Replaced => wire::Why::Replaced,
        Why::Mount => wire::Why::Mount,
        Why::NetworkMount => wire::Why::NetworkMount,
        Why::Unread => wire::Why::Unread,
    }
}

pub fn proof(p: Proof) -> wire::Proof {
    match p {
        Proof::Checked(t) => wire::Proof::Checked(time(t)),
        Proof::AsOf(t) => wire::Proof::AsOf(time(t)),
    }
}

pub fn id(f: FileId) -> wire::Id {
    wire::Id {
        dev: f.dev,
        ino: f.ino,
    }
}

pub fn attrs(a: Attrs) -> wire::Attrs {
    wire::Attrs {
        size: a.size,
        alloc: a.alloc,
        mtime: time(a.mtime),
        ctime: time(a.ctime),
    }
}

/// A hit, its path below `root` made whole.
pub fn hit(root: &Path, h: &Hit) -> wire::Entry {
    wire::Entry {
        path: wire::WirePath::of(&root.join(&h.path)),
        kind: kind(h.kind),
        id: id(h.id),
        folder: id(h.folder),
        attrs: h.attrs.map(attrs),
    }
}

/// An entry of the journal, at `path`, in a folder whose identity is not
/// told by the log.
pub fn entry(path: &Path, e: &Entry) -> wire::Entry {
    wire::Entry {
        path: wire::WirePath::of(path),
        kind: kind(e.kind),
        id: id(e.id),
        folder: wire::Id::default(),
        attrs: e.attrs.map(attrs),
    }
}
