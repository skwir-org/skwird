//! Who asks the system service, and what they may see (`docs/DESIGN.md`,
//! decision 4, phase 3, step 5): their user and every group, as the
//! kernel held them when they connected (`SO_PEERCRED`, `SO_PEERGROUPS`),
//! with no lookup in the user database; each folder's owner, group and
//! mode evaluated against them in the kernel's order.
//!
//! A result is answered only if the asker may list its folder and search
//! every folder above it. A folder with an access ACL is judged by it,
//! as read (`skwird_crawl::acl`); one whose ACL was not read is not
//! known: never answered as its mode alone says, and not covered.
#![cfg_attr(unix, allow(unsafe_code))] // SO_PEERGROUPS and LOCAL_PEERCRED have no safe binding; each block says why it is sound.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::path::Path;

use crate::transport::Stream;

use skwird_index::block::FolderId;
use skwird_index::{Index, Sight};

/// Read, and search: a folder's entries listed, and reached.
const LIST: u16 = 0o5;
/// Search alone: a folder passed through.
const SEARCH: u16 = 0o1;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Access, Token};

/// An asker's token: Windows' alone ([`windows::Token`]); none is ever
/// made elsewhere.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub enum Token {}

/// Who asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asker {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
    /// On Windows, the token its pipe gave, which it is judged by.
    pub token: Option<std::sync::Arc<Token>>,
}

/// A folder's owner, group and mode, the [`skwird_crawl::ACL`] bit in it,
/// and the volume it is on. On Windows its owner is its security id, its
/// group and mode none (DESIGN 4, "On Windows").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Owned {
    pub owner: u32,
    pub group: u32,
    pub mode: u16,
    pub dev: u64,
}

impl Asker {
    /// The peer of `stream`, as the kernel held it when it connected.
    #[cfg(target_os = "linux")]
    pub fn of(stream: &Stream) -> std::io::Result<Asker> {
        let cred = rustix::net::sockopt::socket_peercred(stream)?;
        Ok(Asker {
            uid: cred.uid.as_raw(),
            gid: cred.gid.as_raw(),
            groups: peer_groups(stream)?,
            token: None,
        })
    }

    /// The peer of `stream`, as the kernel held it when it connected
    /// (`LOCAL_PEERCRED`, macOS' `getpeereid` with its groups): its user,
    /// and its groups, the first of which is its own. The kernel gives 16
    /// at most: a peer in more is judged by those alone, which grants it
    /// less, never more.
    #[cfg(target_os = "macos")]
    pub fn of(stream: &Stream) -> std::io::Result<Asker> {
        let mut cred = std::mem::MaybeUninit::<libc::xucred>::zeroed();
        let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::xucred>())
            .unwrap_or(libc::socklen_t::MAX);
        // SAFETY: `cred` is valid for writes of `len` bytes, its size, the
        // most getsockopt writes, and `len` is a word it writes the length
        // to.
        let done = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_LOCAL,
                libc::LOCAL_PEERCRED,
                cred.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        if done != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: zeroed, then written by the call, which succeeded.
        let cred = unsafe { cred.assume_init() };
        if cred.cr_version != libc::XUCRED_VERSION {
            return Err(std::io::Error::other(
                "a peer's credentials of another version",
            ));
        }
        let n = usize::try_from(cred.cr_ngroups)
            .unwrap_or(0)
            .min(cred.cr_groups.len());
        let groups = cred.cr_groups[..n].to_vec();
        Ok(Asker {
            uid: cred.cr_uid,
            gid: groups.first().copied().unwrap_or(u32::MAX),
            groups,
            token: None,
        })
    }

    /// The peer of a pipe: its token, taken as its server's end does
    /// (`skwird_client::pipe::Stream::client_token`), which it is judged
    /// by. Its user is the number the system service gave its SID
    /// (`crate::system::number_of`); refused where the service cannot give
    /// one. A user's engine numbers no one: its one user has none. Its
    /// group ids are none a user has: no mode grants it anything, should it
    /// ever be evaluated.
    #[cfg(windows)]
    pub fn of(stream: &Stream) -> std::io::Result<Asker> {
        let token = Token::of(stream.client_token()?)?;
        let uid = match crate::system::number_of(&token.sid) {
            Some(n) => n,
            None if crate::system::numbering() => {
                return Err(std::io::Error::other("the asker cannot be numbered"));
            }
            None => u32::MAX,
        };
        Ok(Asker {
            uid,
            gid: u32::MAX,
            groups: Vec::new(),
            token: Some(std::sync::Arc::new(token)),
        })
    }

    /// Whether the asker is the user the engine runs as.
    #[cfg(unix)]
    pub fn is_engines_user(&self) -> bool {
        self.uid == rustix::process::geteuid().as_raw()
    }

    /// Whether the asker is the user the engine runs as: on Windows, any
    /// asker the pipe let in.
    #[cfg(windows)]
    pub fn is_engines_user(&self) -> bool {
        match &self.token {
            Some(token) => skwird_client::pipe::user_sid().is_ok_and(|me| me == token.sid),
            None => true,
        }
    }

    /// Whether the asker may see everything: root; on Windows, `LocalSystem`.
    pub fn is_root(&self) -> bool {
        #[cfg(windows)]
        return self.token.as_ref().is_some_and(|t| t.is_system());
        #[cfg(unix)]
        return self.uid == 0;
    }

    /// Whether the asker may do all of `bits` (4 read, 1 search) in a
    /// folder so owned: root may; a folder with an ACL is not known; then
    /// the owner's bits for its owner, the group's for a member of its
    /// group, the others' for anyone else, as the kernel takes them.
    pub fn may(&self, folder: Owned, bits: u16) -> Sight {
        self.may_by(folder, None, bits)
    }

    /// [`Asker::may`], a folder with an ACL judged by `acl`, its ACL as
    /// read: not known if it was not read.
    pub fn may_by(&self, folder: Owned, acl: Option<&skwird_crawl::acl::Acl>, bits: u16) -> Sight {
        if self.is_root() {
            return Sight::Seen;
        }
        // Judged by its descriptor, as Windows judges an open (DESIGN 4).
        #[cfg(windows)]
        {
            let _ = acl;
            let access = if bits == SEARCH {
                Access::Traverse
            } else {
                Access::List
            };
            self.token
                .as_ref()
                .map_or(Sight::Hidden, |t| t.may(folder.dev, folder.owner, access))
        }
        #[cfg(unix)]
        {
            if folder.mode & skwird_crawl::ACL != 0 {
                return match acl {
                    Some(acl) => self.judged(folder, acl, bits),
                    None => Sight::Unknown,
                };
            }
            self.by_mode(folder, bits)
        }
    }

    /// Linux's: a folder with an access ACL judged by it alone (acl(5)).
    #[cfg(all(unix, not(target_os = "macos")))]
    fn judged(&self, folder: Owned, acl: &skwird_crawl::acl::Acl, bits: u16) -> Sight {
        if acl.allows(
            self.uid,
            self.gid,
            &self.groups,
            folder.owner,
            folder.group,
            bits,
        ) {
            Sight::Seen
        } else {
            Sight::Hidden
        }
    }

    /// macOS': a folder with an extended ACL judged by it first, then by its
    /// mode for what it left (DESIGN 4, kauth's order), its entries matched
    /// against the asker by the system's membership calls; not known where
    /// the system cannot tell.
    #[cfg(target_os = "macos")]
    fn judged(&self, folder: Owned, acl: &skwird_crawl::acl::Extended, bits: u16) -> Sight {
        use skwird_crawl::acl::{Decided, LIST_DIRECTORY, SEARCH as PASS};
        let rights = |bits: u16| {
            (if bits & 0o4 != 0 { LIST_DIRECTORY } else { 0 })
                | (if bits & 0o1 != 0 { PASS } else { 0 })
        };
        let me = std::cell::OnceCell::new();
        match acl.decide(rights(bits), |who| membership::applies(self.uid, &me, who)) {
            None => Sight::Unknown,
            Some(Decided::Denied) => Sight::Hidden,
            Some(Decided::Allowed) => Sight::Seen,
            Some(Decided::Left(left)) => {
                let bits = (if left & LIST_DIRECTORY != 0 { 0o4 } else { 0 })
                    | u16::from(left & PASS != 0);
                self.by_mode(folder, bits)
            }
        }
    }

    /// The owner's bits for its owner, the group's for a member of its
    /// group, the others' for anyone else, as the kernel takes them. On
    /// macOS, an asker the socket gave the most groups of (16) may be in
    /// more: the system is asked whether they are in the folder's, or
    /// the others' bits could grant what the group's would not.
    #[cfg(unix)]
    fn by_mode(&self, folder: Owned, bits: u16) -> Sight {
        let member = self.gid == folder.group || self.groups.contains(&folder.group);
        #[cfg(target_os = "macos")]
        let member = if member || self.groups.len() < membership::CRED_GROUPS {
            member
        } else {
            match membership::in_group(self.uid, folder.group) {
                Some(member) => member,
                None => return Sight::Unknown,
            }
        };
        let granted = if self.uid == folder.owner {
            folder.mode >> 6
        } else if member {
            folder.mode >> 3
        } else {
            folder.mode
        };
        if granted & bits == bits {
            Sight::Seen
        } else {
            Sight::Hidden
        }
    }
}

/// Who is in which group, as macOS' directory says (membership.h): the
/// calls the kernel's own judging asks, nested groups and `everyone`
/// among them. No copy is kept: the system keeps its own.
#[cfg(target_os = "macos")]
mod membership {
    /// The most groups a socket's peer is given with (`XU_NGROUPS`).
    pub const CRED_GROUPS: usize = 16;
    /// `ID_TYPE_UID`: a UUID that is a user's.
    const ID_TYPE_UID: libc::c_int = 0;

    unsafe extern "C" {
        fn mbr_uid_to_uuid(uid: libc::uid_t, uu: *mut u8) -> libc::c_int;
        fn mbr_gid_to_uuid(gid: libc::gid_t, uu: *mut u8) -> libc::c_int;
        fn mbr_uuid_to_id(
            uu: *const u8,
            id: *mut libc::id_t,
            kind: *mut libc::c_int,
        ) -> libc::c_int;
        fn mbr_check_membership(
            user: *const u8,
            group: *const u8,
            ismember: *mut libc::c_int,
        ) -> libc::c_int;
    }

    /// User `uid`'s UUID.
    pub fn user(uid: u32) -> Option<[u8; 16]> {
        let mut uu = [0u8; 16];
        // SAFETY: a buffer of the 16 bytes of a UUID, which the call fills.
        (unsafe { mbr_uid_to_uuid(uid, uu.as_mut_ptr()) } == 0).then_some(uu)
    }

    /// Whether user `me` is in the group `group` names.
    fn member(me: &[u8; 16], group: &[u8; 16]) -> Option<bool> {
        let mut ismember: libc::c_int = 0;
        // SAFETY: two UUIDs of 16 bytes each; a word the call writes.
        let done = unsafe { mbr_check_membership(me.as_ptr(), group.as_ptr(), &raw mut ismember) };
        (done == 0).then_some(ismember != 0)
    }

    /// Whether an entry naming `who` applies to user `uid` (their UUID in
    /// `me`, asked once): they are who it names, or in the group it names.
    /// One that names another user is not theirs. None where the system
    /// cannot tell.
    pub fn applies(
        uid: u32,
        me: &std::cell::OnceCell<Option<[u8; 16]>>,
        who: &[u8; 16],
    ) -> Option<bool> {
        if *who == skwird_crawl::acl::EVERYONE {
            return Some(true);
        }
        let me = (*me.get_or_init(|| user(uid)))?;
        if me == *who {
            return Some(true);
        }
        if let Some(member) = member(&me, who) {
            return Some(member);
        }
        let (mut id, mut kind): (libc::id_t, libc::c_int) = (0, -1);
        // SAFETY: a UUID of 16 bytes; two words the call writes.
        let known = unsafe { mbr_uuid_to_id(who.as_ptr(), &raw mut id, &raw mut kind) } == 0;
        (known && kind == ID_TYPE_UID).then_some(false)
    }

    /// Whether user `uid` is in group `gid`.
    pub fn in_group(uid: u32, gid: u32) -> Option<bool> {
        let me = user(uid)?;
        let mut group = [0u8; 16];
        // SAFETY: a buffer of the 16 bytes of a UUID, which the call fills.
        if unsafe { mbr_gid_to_uuid(gid, group.as_mut_ptr()) } != 0 {
            return None;
        }
        member(&me, &group)
    }
}

/// The peer's groups, every one, as the kernel holds them.
#[cfg(target_os = "linux")]
fn peer_groups(stream: &Stream) -> std::io::Result<Vec<u32>> {
    let mut groups = vec![0u32; 64];
    loop {
        let mut len = libc::socklen_t::try_from(groups.len() * 4).unwrap_or(libc::socklen_t::MAX);
        // SAFETY: `groups` is valid for writes of `len` bytes, the most
        // getsockopt writes, and `len` is a word it writes the length to.
        let done = unsafe {
            libc::getsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERGROUPS,
                groups.as_mut_ptr().cast(),
                &raw mut len,
            )
        };
        let wanted = len as usize / 4;
        if done == 0 {
            groups.truncate(wanted);
            return Ok(groups);
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ERANGE) && wanted > groups.len() {
            groups.resize(wanted, 0);
            continue;
        }
        return Err(e);
    }
}

/// What a folder at `path` is owned as, by its stat now, following no
/// link: none if it cannot be stat-ed. Its ACL is asked too.
#[cfg(unix)]
pub fn owned_at(path: &Path) -> Option<Owned> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(path).ok()?;
    let acl = has_acl(path);
    Some(Owned {
        owner: meta.uid(),
        group: meta.gid(),
        mode: (meta.mode() & 0o7777) as u16 | if acl { skwird_crawl::ACL } else { 0 },
        // As the crawl keeps it: macOS gives it signed.
        #[cfg(target_os = "macos")]
        dev: u64::from(meta.dev() as u32),
        #[cfg(not(target_os = "macos"))]
        dev: meta.dev(),
    })
}

/// Whether the folder at `path` has an access ACL, as Linux keeps one: one
/// that cannot be asked may.
#[cfg(target_os = "linux")]
fn has_acl(path: &Path) -> bool {
    let mut none: [u8; 0] = [];
    match rustix::fs::lgetxattr(path, "system.posix_acl_access", &mut none[..]) {
        Ok(len) => len > 0,
        Err(rustix::io::Errno::NODATA | rustix::io::Errno::NOTSUP) => false,
        Err(_) => true,
    }
}

/// Whether the folder at `path` has an ACL, an extended one as macOS keeps
/// it: one that cannot be asked may.
#[cfg(target_os = "macos")]
fn has_acl(path: &Path) -> bool {
    skwird_crawl::acl_at(path)
}

/// What `asker` may see of each folder's entries in `index`: whether they
/// may list it and search every folder above it, the folders above the
/// index's root being `above`. Remembered in `memo`, by folder, as how far
/// the asker reaches: each folder above a result is evaluated once.
pub(crate) fn sight(
    index: &Index,
    asker: &Asker,
    above: Sight,
    folder: FolderId,
    memo: &mut HashMap<FolderId, Sight>,
) -> Sight {
    let Some(facts) = index.facts_of(folder) else {
        return Sight::Hidden;
    };
    let owned = Owned {
        owner: facts.owner,
        group: facts.group,
        mode: facts.mode,
        dev: facts.id.dev,
    };
    asker.may_by(owned, index.acl_of(facts.id), LIST).and(reach(
        index,
        asker,
        above,
        index.parent_of(folder),
        memo,
    ))
}

/// Whether `asker` may search `folder` and every folder above it.
fn reach(
    index: &Index,
    asker: &Asker,
    above: Sight,
    folder: Option<FolderId>,
    memo: &mut HashMap<FolderId, Sight>,
) -> Sight {
    let mut chain = Vec::new();
    let mut at = folder;
    let mut base = above;
    while let Some(f) = at {
        if let Some(&known) = memo.get(&f) {
            base = known;
            break;
        }
        chain.push(f);
        at = index.parent_of(f);
    }
    for f in chain.into_iter().rev() {
        let searched = index.facts_of(f).map_or(Sight::Hidden, |facts| {
            asker.may_by(
                Owned {
                    owner: facts.owner,
                    group: facts.group,
                    mode: facts.mode,
                    dev: facts.id.dev,
                },
                index.acl_of(facts.id),
                SEARCH,
            )
        });
        base = base.and(searched);
        memo.insert(f, base);
    }
    base
}

/// An asker's view of one root's index.
pub struct Seeing<'a> {
    pub index: &'a Index,
    pub asker: &'a Asker,
    /// Whether they may search every folder above the root.
    pub above: Sight,
}

impl Seeing<'_> {
    /// What they may see of `folder`'s entries ([`sight`]).
    pub(crate) fn sight(&self, folder: FolderId, memo: &mut HashMap<FolderId, Sight>) -> Sight {
        sight(self.index, self.asker, self.above, folder, memo)
    }

    /// The folders with an ACL below `folder` whose names they see: not
    /// covered for them, until ACLs are evaluated.
    pub(crate) fn acl_holes(
        &self,
        folder: FolderId,
        memo: &mut HashMap<FolderId, Sight>,
    ) -> Vec<FolderId> {
        // Those whose ACL was not read alone: one read is judged by it.
        self.index
            .acl_folders_below(folder)
            .into_iter()
            .filter(|&f| f != folder)
            .filter(|&f| {
                self.index
                    .facts_of(f)
                    .is_some_and(|facts| self.index.acl_of(facts.id).is_none())
            })
            .filter(|&f| {
                self.index
                    .parent_of(f)
                    .is_some_and(|p| self.sight(p, memo) == Sight::Seen)
            })
            .collect()
    }

    /// Of `holes`, holes of this index's root `root`, those the asker may
    /// know of: a hole is a folder's name, said only to who may list the
    /// folder holding it, as its own listing would.
    pub(crate) fn seen_holes(
        &self,
        root: &Path,
        holes: &mut Vec<skwird_protocol::Hole>,
        memo: &mut HashMap<FolderId, Sight>,
    ) {
        holes.retain(|h| {
            let path = h.path.to_path();
            let Ok(rel) = path.strip_prefix(root) else {
                return false;
            };
            let parent = rel.parent().unwrap_or(Path::new(""));
            self.index
                .folder_at(parent)
                .is_some_and(|p| self.sight(p, memo) == Sight::Seen)
        });
    }
}

/// Whether `asker` may search every folder in `above`.
pub fn through(asker: &Asker, above: &[Option<Owned>]) -> Sight {
    above.iter().fold(Sight::Seen, |s, f| {
        s.and(f.map_or(Sight::Hidden, |f| asker.may(f, SEARCH)))
    })
}

// Unix's modes and ACLs: Windows' judging is by token (`windows`).
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn asker(uid: u32, gid: u32, groups: &[u32]) -> Asker {
        Asker {
            uid,
            gid,
            groups: groups.to_vec(),
            token: None,
        }
    }

    fn folder(owner: u32, group: u32, mode: u16) -> Owned {
        Owned {
            owner,
            group,
            mode,
            dev: 0,
        }
    }

    /// The owner's bits for the owner, even where the group's or the
    /// others' would give more; the group's for a member, by its own
    /// group or another; the others' for anyone else; root may.
    #[test]
    fn modes_are_taken_as_the_kernel_takes_them() {
        let someone = asker(1000, 100, &[]);
        let member = asker(1001, 500, &[100, 7]);
        let stranger = asker(1002, 500, &[]);
        let root = asker(0, 0, &[]);
        let f = folder(1000, 100, 0o075);
        assert_eq!(someone.may(f, LIST), Sight::Hidden, "the owner's bits");
        assert_eq!(member.may(f, LIST), Sight::Seen, "the group's bits");
        assert_eq!(stranger.may(f, LIST), Sight::Seen, "the others' bits");
        assert_eq!(root.may(folder(1, 1, 0), LIST), Sight::Seen);
        let g = folder(1000, 100, 0o711);
        assert_eq!(member.may(g, SEARCH), Sight::Seen);
        assert_eq!(member.may(g, LIST), Sight::Hidden, "search alone");
        assert_eq!(
            asker(1003, 7, &[]).may(folder(1, 7, 0o750), LIST),
            Sight::Seen
        );
    }

    /// A folder with an ACL is not known, to anyone but root.
    #[test]
    fn a_folder_with_an_acl_is_not_known() {
        let f = folder(1000, 100, 0o777 | skwird_crawl::ACL);
        assert_eq!(asker(1000, 100, &[]).may(f, LIST), Sight::Unknown);
        assert_eq!(asker(0, 0, &[]).may(f, LIST), Sight::Seen);
    }

    /// A folder with an ACL read is judged by it: the user it names may,
    /// another may not (Linux's).
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn a_folder_with_an_acl_read_is_judged_by_it() {
        // user::rwx, user:1001:r-x, group::---, mask::r-x, other::---.
        let mut bytes = 2u32.to_le_bytes().to_vec();
        for (tag, perm, id) in [
            (1u16, 7u16, u32::MAX),
            (2, 5, 1001),
            (4, 0, u32::MAX),
            (0x10, 5, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&perm.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        let acl = skwird_crawl::acl::Acl::parse(&bytes).unwrap();
        let f = folder(0, 0, 0o750 | skwird_crawl::ACL);
        assert_eq!(
            asker(1001, 1001, &[]).may_by(f, Some(&acl), LIST),
            Sight::Seen
        );
        assert_eq!(
            asker(1002, 1002, &[]).may_by(f, Some(&acl), LIST),
            Sight::Hidden
        );
    }

    /// Every folder above must be searchable; one hidden hides, one not
    /// known leaves it not known.
    #[test]
    fn every_folder_above_must_be_searched() {
        let someone = asker(1000, 100, &[]);
        let open = Some(folder(0, 0, 0o755));
        let closed = Some(folder(0, 0, 0o700));
        let acl = Some(folder(0, 0, 0o755 | skwird_crawl::ACL));
        assert_eq!(through(&someone, &[open, open]), Sight::Seen);
        assert_eq!(through(&someone, &[open, closed]), Sight::Hidden);
        assert_eq!(through(&someone, &[acl, open]), Sight::Unknown);
        assert_eq!(through(&someone, &[acl, closed]), Sight::Hidden);
        assert_eq!(through(&someone, &[None]), Sight::Hidden, "not stat-ed");
    }

    /// macOS' extended ACL as `getattrlist` gives it, of `entries` (a UUID,
    /// flags, rights).
    #[cfg(target_os = "macos")]
    fn extended(entries: &[([u8; 16], u32, u32)]) -> skwird_crawl::acl::Extended {
        let mut b = 0x012c_c16d_u32.to_le_bytes().to_vec();
        b.extend([0u8; 32]);
        b.extend(u32::try_from(entries.len()).unwrap().to_le_bytes());
        b.extend(0u32.to_le_bytes());
        for (who, flags, rights) in entries {
            b.extend(who);
            b.extend(flags.to_le_bytes());
            b.extend(rights.to_le_bytes());
        }
        skwird_crawl::acl::Extended::parse(&b).unwrap()
    }

    /// On macOS, a folder with an extended ACL is judged by it first, then
    /// by its mode for what it leaves: a deny of listing hides what the
    /// mode would show, and leaves passing through to the mode; an allow
    /// shows what the mode would hide; the default entry, deletion's,
    /// leaves it all to the mode. An entry naming a user by the UUID the
    /// system gives them applies to them, not to another.
    #[cfg(target_os = "macos")]
    #[test]
    fn on_macos_a_folder_is_judged_by_its_extended_acl_then_its_mode() {
        use skwird_crawl::acl::{EVERYONE, LIST_DIRECTORY, SEARCH as PASS};
        const PERMIT: u32 = 1;
        const DENY: u32 = 2;
        const DELETE: u32 = 1 << 4;
        let someone = asker(1000, 20, &[20]);
        let open = folder(0, 0, 0o755 | skwird_crawl::ACL);
        let closed = folder(0, 0, 0o700 | skwird_crawl::ACL);
        let deny_list = extended(&[(EVERYONE, DENY, LIST_DIRECTORY)]);
        assert_eq!(someone.may_by(open, Some(&deny_list), LIST), Sight::Hidden);
        assert_eq!(
            someone.may_by(open, Some(&deny_list), SEARCH),
            Sight::Seen,
            "passing through left to the mode"
        );
        let allow = extended(&[(EVERYONE, PERMIT, LIST_DIRECTORY | PASS)]);
        assert_eq!(someone.may_by(closed, Some(&allow), LIST), Sight::Seen);
        let default = extended(&[(EVERYONE, DENY, DELETE)]);
        assert_eq!(someone.may_by(open, Some(&default), LIST), Sight::Seen);
        assert_eq!(someone.may_by(closed, Some(&default), LIST), Sight::Hidden);
        // This test's user, by the UUID the system gives them.
        let uid = rustix::process::getuid().as_raw();
        let me = membership::user(uid).expect("this user's UUID");
        let mine = extended(&[(me, DENY, LIST_DIRECTORY)]);
        assert_eq!(
            asker(uid, 20, &[20]).may_by(open, Some(&mine), LIST),
            Sight::Hidden
        );
        // `nobody` is not them.
        let nobody = asker(u32::MAX - 1, u32::MAX - 1, &[]);
        assert_eq!(nobody.may_by(open, Some(&mine), LIST), Sight::Seen);
    }
}
