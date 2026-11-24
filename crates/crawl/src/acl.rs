//! A folder's access ACL (`docs/DESIGN.md`, decision 4): read as its
//! system keeps it, and evaluated as its kernel evaluates it, so that the
//! system service answers each asker what a folder with an ACL lets them
//! list.
//! - Linux's ([`Posix`], phase 3, step 5b): `system.posix_acl_access`,
//!   acl(5).
//! - macOS' ([`Extended`], phase 5, step 7): an extended ACL, as
//!   `getattrlist` gives it (`ATTR_CMN_EXTENDED_SECURITY`), evaluated as
//!   kauth does.
//!
//! Both are read and tested on every system; [`Acl`] is the one this
//! system keeps.

/// The ACL this system keeps: macOS' extended one.
#[cfg(target_os = "macos")]
pub type Acl = Extended;
/// The ACL this system keeps: Linux's access ACL.
#[cfg(not(target_os = "macos"))]
pub type Acl = Posix;

/// The tags of an ACL's entries, as the kernel writes them.
const USER_OBJ: u16 = 0x01;
const USER: u16 = 0x02;
const GROUP_OBJ: u16 = 0x04;
const GROUP: u16 = 0x08;
const MASK: u16 = 0x10;
const OTHER: u16 = 0x20;

/// The version the kernel writes an ACL's attribute in.
const VERSION: u32 = 2;

/// An access ACL: each entry's tag, permissions (4 read, 2 write, 1
/// search) and the user or group it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Posix {
    entries: Vec<(u16, u16, u32)>,
}

impl Posix {
    /// An ACL as the kernel writes it in `system.posix_acl_access`: a
    /// version, then entries of a tag, permissions and an id. None if it
    /// is not one, or lacks an entry every ACL has.
    pub fn parse(bytes: &[u8]) -> Option<Posix> {
        let (version, rest) = bytes.split_first_chunk::<4>()?;
        if u32::from_le_bytes(*version) != VERSION || rest.len() % 8 != 0 {
            return None;
        }
        let entries: Vec<(u16, u16, u32)> = rest
            .chunks_exact(8)
            .map(|e| {
                (
                    u16::from_le_bytes([e[0], e[1]]),
                    u16::from_le_bytes([e[2], e[3]]),
                    u32::from_le_bytes([e[4], e[5], e[6], e[7]]),
                )
            })
            .collect();
        let has = |tag| entries.iter().any(|&(t, ..)| t == tag);
        (has(USER_OBJ) && has(GROUP_OBJ) && has(OTHER)).then_some(Posix { entries })
    }

    fn perm(&self, tag: u16) -> Option<u16> {
        self.entries
            .iter()
            .find(|&&(t, ..)| t == tag)
            .map(|&(_, p, _)| p)
    }

    /// Whether a user (`uid`, primary group `gid`, and `groups`) may do
    /// all of `bits` in a folder of this ACL, owned by `owner` and
    /// `group`, as the kernel decides (acl(5)): its owner by the owner's
    /// entry; a user named by their entry, within the mask; a member of
    /// the owning group or of a named group by any such entry that grants
    /// it, within the mask, and refused if none does, never falling to the
    /// others'; anyone else by the others' entry.
    pub fn allows(
        &self,
        uid: u32,
        gid: u32,
        groups: &[u32],
        owner: u32,
        group: u32,
        bits: u16,
    ) -> bool {
        let mask = self.perm(MASK).unwrap_or(0o7);
        let grants = |perm: u16| perm & bits == bits;
        if uid == owner {
            return self.perm(USER_OBJ).is_some_and(grants);
        }
        if let Some(&(_, perm, _)) = self
            .entries
            .iter()
            .find(|&&(t, _, id)| t == USER && id == uid)
        {
            return grants(perm & mask);
        }
        let member = |g: u32| g == gid || groups.contains(&g);
        let mut matched = false;
        for &(tag, perm, id) in &self.entries {
            let named = match tag {
                GROUP_OBJ => member(group),
                GROUP => member(id),
                _ => false,
            };
            if named {
                matched = true;
                if grants(perm & mask) {
                    return true;
                }
            }
        }
        if matched {
            return false;
        }
        self.perm(OTHER).is_some_and(grants)
    }
}

/// What an extended ACL's entry is (`KAUTH_ACE_KINDMASK` of its flags): an
/// allow or a deny. An audit or an alarm decides nothing.
const KIND: u32 = 0xf;
const PERMIT: u32 = 1;
const DENY: u32 = 2;
/// An entry for what is made inside alone, not for the folder itself
/// (`KAUTH_ACE_ONLY_INHERIT`).
const ONLY_INHERIT: u32 = 1 << 8;
/// What an extended security's first word is (`KAUTH_FILESEC_MAGIC`).
const FILESEC_MAGIC: u32 = 0x012c_c16d;
/// A count of entries that says there is no ACL (`KAUTH_FILESEC_NOACL`).
const NO_ACL: u32 = u32::MAX;
/// Where an extended security's entries start: after its magic, its
/// owner's and group's UUIDs, and its count and flags.
const ENTRIES_AT: usize = 4 + 16 + 16 + 4 + 4;

/// A folder's right to have its entries listed (`KAUTH_VNODE_LIST_DIRECTORY`).
pub const LIST_DIRECTORY: u32 = 1 << 1;
/// A folder's right to be passed through (`KAUTH_VNODE_SEARCH`).
pub const SEARCH: u32 = 1 << 3;
/// The generic rights an entry may name (`KAUTH_ACE_GENERIC_*`), each
/// holding some of those.
const GENERIC_ALL: u32 = 1 << 21;
const GENERIC_EXECUTE: u32 = 1 << 22;
const GENERIC_READ: u32 = 1 << 24;

/// The UUID of `everyone`, the group every user is in
/// (`ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C`).
pub const EVERYONE: [u8; 16] = [
    0xab, 0xcd, 0xef, 0xab, 0xcd, 0xef, 0xab, 0xcd, 0xef, 0xab, 0xcd, 0xef, 0, 0, 0, 0x0c,
];

/// macOS' extended ACL: its entries in order, each naming a user or a
/// group by UUID, its kind and flags, and the rights it names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Extended {
    entries: Vec<Entry>,
}

/// An entry of an extended ACL (`kauth_ace`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The user or group it applies to.
    pub who: [u8; 16],
    pub flags: u32,
    pub rights: u32,
}

/// What an extended ACL decides of the rights asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decided {
    /// An entry that applies denies one of them.
    Denied,
    /// The entries that apply allow them all.
    Allowed,
    /// These, which no entry decided, are left to the mode.
    Left(u32),
}

impl Extended {
    /// An extended ACL as `getattrlist` gives it (a `kauth_filesec`): its
    /// magic, its owner's and its group's UUIDs, its count of entries and
    /// its flags, then each entry, a UUID, flags and rights, each word in
    /// the byte order its magic is in. None if it is not one; one of no
    /// entries where it says it has none.
    pub fn parse(bytes: &[u8]) -> Option<Extended> {
        let word = |at: usize, big: bool| -> Option<u32> {
            let w: [u8; 4] = bytes.get(at..at.checked_add(4)?)?.try_into().ok()?;
            Some(if big {
                u32::from_be_bytes(w)
            } else {
                u32::from_le_bytes(w)
            })
        };
        let big = if word(0, false)? == FILESEC_MAGIC {
            false
        } else if word(0, true)? == FILESEC_MAGIC {
            true
        } else {
            return None;
        };
        let count = word(ENTRIES_AT - 8, big)?;
        if count == NO_ACL {
            return Some(Extended {
                entries: Vec::new(),
            });
        }
        let entries = (0..usize::try_from(count).ok()?)
            .map(|i| {
                let at = ENTRIES_AT.checked_add(i.checked_mul(24)?)?;
                let who: [u8; 16] = bytes.get(at..at.checked_add(16)?)?.try_into().ok()?;
                Some(Entry {
                    who,
                    flags: word(at + 16, big)?,
                    rights: word(at + 20, big)?,
                })
            })
            .collect::<Option<Vec<Entry>>>()?;
        Some(Extended { entries })
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// What the entries decide of the rights `asked` (of [`LIST_DIRECTORY`]
    /// and [`SEARCH`]), as kauth decides them (`kauth_acl_evaluate`), in
    /// order:
    /// - an entry for what is made inside alone is passed over, and so is
    ///   one that names none of the rights still to be decided, so that
    ///   `applies` is asked of no other: the default ones, which name
    ///   `delete` alone, never;
    /// - an allow that applies takes the rights it names off those left,
    ///   all allowed once none is;
    /// - a deny that applies, of any right asked, refuses.
    ///
    /// What no entry decided is left to the mode. None where `applies`
    /// cannot tell whether an entry is the asker's: the ACL is never
    /// judged as if it were not.
    pub fn decide(
        &self,
        asked: u32,
        mut applies: impl FnMut(&[u8; 16]) -> Option<bool>,
    ) -> Option<Decided> {
        let mut left = asked;
        for e in &self.entries {
            if e.flags & ONLY_INHERIT != 0 {
                continue;
            }
            let rights = expanded(e.rights);
            let kind = e.flags & KIND;
            let names = match kind {
                PERMIT => rights & left != 0,
                DENY => rights & asked != 0,
                _ => false,
            };
            if !names || !applies(&e.who)? {
                continue;
            }
            if kind == DENY {
                return Some(Decided::Denied);
            }
            left &= !rights;
            if left == 0 {
                return Some(Decided::Allowed);
            }
        }
        Some(Decided::Left(left))
    }
}

/// The rights an entry names, its generic ones held as those they hold of
/// a folder's listing and passing through (`ae_exp_g*`, as vnode
/// authorization gives them).
fn expanded(rights: u32) -> u32 {
    let mut r = rights;
    if rights & GENERIC_ALL != 0 {
        r |= LIST_DIRECTORY | SEARCH;
    }
    if rights & GENERIC_READ != 0 {
        r |= LIST_DIRECTORY;
    }
    if rights & GENERIC_EXECUTE != 0 {
        r |= SEARCH;
    }
    r
}

#[cfg(test)]
mod tests {
    use super::Posix as Acl;
    use super::*;

    fn acl(entries: &[(u16, u16, u32)]) -> Acl {
        let mut bytes = VERSION.to_le_bytes().to_vec();
        for &(tag, perm, id) in entries {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&perm.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        Acl::parse(&bytes).unwrap()
    }

    const UNDEFINED: u32 = u32::MAX;
    const LIST: u16 = 0o5;

    /// A folder 0700, owned by root, granting one user and one group
    /// read and search, within a mask that keeps them.
    fn granted() -> Acl {
        acl(&[
            (USER_OBJ, 0o7, UNDEFINED),
            (USER, 0o5, 1001),
            (GROUP_OBJ, 0o0, UNDEFINED),
            (GROUP, 0o5, 2000),
            (MASK, 0o5, UNDEFINED),
            (OTHER, 0o0, UNDEFINED),
        ])
    }

    #[test]
    fn a_named_user_and_a_named_group_are_granted() {
        let a = granted();
        assert!(a.allows(1001, 1001, &[], 0, 0, LIST), "the named user");
        assert!(
            a.allows(1002, 1002, &[2000], 0, 0, LIST),
            "in the named group"
        );
        assert!(!a.allows(1003, 1003, &[], 0, 0, LIST), "anyone else");
        assert!(a.allows(0, 0, &[], 0, 0, LIST), "its owner");
    }

    #[test]
    fn the_mask_bounds_the_named_entries() {
        let a = acl(&[
            (USER_OBJ, 0o7, UNDEFINED),
            (USER, 0o7, 1001),
            (GROUP_OBJ, 0o5, UNDEFINED),
            (MASK, 0o1, UNDEFINED),
            (OTHER, 0o0, UNDEFINED),
        ]);
        assert!(!a.allows(1001, 1001, &[], 0, 0, LIST), "read masked out");
        assert!(a.allows(1001, 1001, &[], 0, 0, 0o1), "search kept");
        assert!(
            !a.allows(1002, 50, &[], 0, 50, LIST),
            "the owning group, masked"
        );
    }

    /// A member of a group the ACL names, refused by it, is not given the
    /// others' permissions; the owner is judged by the owner's entry
    /// alone.
    #[test]
    fn a_matching_group_never_falls_to_the_others() {
        let a = acl(&[
            (USER_OBJ, 0o0, UNDEFINED),
            (GROUP_OBJ, 0o0, UNDEFINED),
            (GROUP, 0o0, 2000),
            (MASK, 0o7, UNDEFINED),
            (OTHER, 0o5, UNDEFINED),
        ]);
        assert!(
            !a.allows(1002, 1002, &[2000], 7, 7, LIST),
            "a refused member"
        );
        assert!(a.allows(1003, 1003, &[], 7, 7, LIST), "anyone else");
        assert!(
            !a.allows(7, 7, &[], 7, 7, LIST),
            "its owner, by the owner's entry"
        );
    }

    /// The bytes Linux 6.18 wrote for `setfacl -m u:65534:rx,g:2000:r,m::rx`
    /// on a folder 0755.
    #[test]
    fn an_acl_is_read_as_the_kernel_writes_it() {
        let bytes = [
            0x02, 0, 0, 0, 0x01, 0, 0x07, 0, 0xff, 0xff, 0xff, 0xff, 0x02, 0, 0x05, 0, 0xfe, 0xff,
            0, 0, 0x04, 0, 0x05, 0, 0xff, 0xff, 0xff, 0xff, 0x08, 0, 0x04, 0, 0xd0, 0x07, 0, 0,
            0x10, 0, 0x05, 0, 0xff, 0xff, 0xff, 0xff, 0x20, 0, 0x05, 0, 0xff, 0xff, 0xff, 0xff,
        ];
        let a = Acl::parse(&bytes).unwrap();
        assert!(a.allows(65534, 65534, &[], 1000, 100, LIST));
        assert!(
            !a.allows(1001, 1001, &[2000], 1000, 100, LIST),
            "the group reads alone"
        );
        assert!(a.allows(1001, 1001, &[2000], 1000, 100, 0o4));
    }

    #[test]
    fn what_is_not_an_acl_is_refused() {
        assert_eq!(Acl::parse(&[]), None);
        assert_eq!(Acl::parse(&3u32.to_le_bytes()), None, "another version");
        assert_eq!(Acl::parse(&[2, 0, 0, 0, 1, 0]), None, "cut short");
        let mut no_other = VERSION.to_le_bytes().to_vec();
        no_other.extend_from_slice(&[1, 0, 7, 0, 255, 255, 255, 255]);
        no_other.extend_from_slice(&[4, 0, 5, 0, 255, 255, 255, 255]);
        assert_eq!(Acl::parse(&no_other), None, "no others' entry");
    }

    /// An extended security as `getattrlist` gives it, in `big` or little
    /// byte order: its magic, two UUIDs, the count of `entries`, no flags,
    /// then each entry.
    fn security(entries: &[([u8; 16], u32, u32)], big: bool) -> Vec<u8> {
        let w = |v: u32| {
            if big {
                v.to_be_bytes()
            } else {
                v.to_le_bytes()
            }
        };
        let mut b = w(FILESEC_MAGIC).to_vec();
        b.extend([0u8; 32]);
        b.extend(w(u32::try_from(entries.len()).unwrap()));
        b.extend(w(0));
        for (who, flags, rights) in entries {
            b.extend(who);
            b.extend(w(*flags));
            b.extend(w(*rights));
        }
        b
    }

    const DELETE: u32 = 1 << 4;
    const SOMEONE: [u8; 16] = [7; 16];
    const A_GROUP: [u8; 16] = [9; 16];

    fn extended(entries: &[([u8; 16], u32, u32)]) -> Extended {
        Extended::parse(&security(entries, false)).unwrap()
    }

    /// An extended ACL is read in the byte order its magic is in; one that
    /// says it has none has no entry; what is not one, or is cut short, is
    /// refused.
    #[test]
    fn an_extended_acl_is_read_in_its_magics_byte_order() {
        for big in [false, true] {
            let bytes = security(
                &[
                    (EVERYONE, DENY, DELETE),
                    (SOMEONE, PERMIT, LIST_DIRECTORY | SEARCH),
                ],
                big,
            );
            let acl = Extended::parse(&bytes).unwrap();
            assert_eq!(
                acl.entries(),
                &[
                    Entry {
                        who: EVERYONE,
                        flags: DENY,
                        rights: DELETE
                    },
                    Entry {
                        who: SOMEONE,
                        flags: PERMIT,
                        rights: LIST_DIRECTORY | SEARCH
                    },
                ]
            );
        }
        let mut none = security(&[], false);
        none[36..40].copy_from_slice(&NO_ACL.to_le_bytes());
        assert_eq!(Extended::parse(&none).unwrap().entries(), &[]);
        assert_eq!(Extended::parse(&[0; 44]), None, "no magic");
        let whole = security(&[(SOMEONE, PERMIT, SEARCH)], false);
        assert_eq!(
            Extended::parse(&whole[..whole.len() - 1]),
            None,
            "cut short"
        );
    }

    /// The default entries name deletion alone: passed over, no asker
    /// looked up, the mode left to decide.
    #[test]
    fn an_entry_that_names_no_right_asked_is_passed_over() {
        let acl = extended(&[(EVERYONE, DENY, DELETE)]);
        let decided = acl.decide(LIST_DIRECTORY | SEARCH, |_| panic!("looked up"));
        assert_eq!(decided, Some(Decided::Left(LIST_DIRECTORY | SEARCH)));
    }

    /// In order: an allow takes off what it names, the rest left to the
    /// mode; a deny of a right asked refuses, after an allow of another;
    /// an allow of every right asked, before a deny, allows. One not the
    /// asker's, or for what is made inside alone, is passed over; a
    /// generic right holds those it holds.
    #[test]
    fn entries_decide_in_their_order() {
        let me = |who: &[u8; 16]| Some(*who == SOMEONE || *who == EVERYONE);
        let both = LIST_DIRECTORY | SEARCH;
        let decide = |entries: &[([u8; 16], u32, u32)], asked| extended(entries).decide(asked, me);
        assert_eq!(
            decide(&[(SOMEONE, PERMIT, LIST_DIRECTORY)], both),
            Some(Decided::Left(SEARCH))
        );
        assert_eq!(
            decide(
                &[
                    (SOMEONE, PERMIT, LIST_DIRECTORY),
                    (EVERYONE, DENY, LIST_DIRECTORY)
                ],
                both
            ),
            Some(Decided::Denied)
        );
        assert_eq!(
            decide(
                &[(SOMEONE, PERMIT, both), (EVERYONE, DENY, LIST_DIRECTORY)],
                both
            ),
            Some(Decided::Allowed)
        );
        assert_eq!(
            decide(&[(A_GROUP, DENY, both)], both),
            Some(Decided::Left(both))
        );
        assert_eq!(
            decide(&[(SOMEONE, DENY | ONLY_INHERIT, both)], both),
            Some(Decided::Left(both))
        );
        assert_eq!(
            decide(
                &[
                    (SOMEONE, PERMIT, GENERIC_READ),
                    (SOMEONE, PERMIT, GENERIC_EXECUTE)
                ],
                both
            ),
            Some(Decided::Allowed)
        );
        assert_eq!(
            decide(&[(SOMEONE, DENY, GENERIC_ALL)], SEARCH),
            Some(Decided::Denied)
        );
    }

    /// An entry whose asker cannot be told leaves the ACL undecided: never
    /// judged as if it were not theirs.
    #[test]
    fn an_entry_that_cannot_be_told_decides_nothing() {
        let acl = extended(&[(A_GROUP, DENY, LIST_DIRECTORY)]);
        assert_eq!(acl.decide(LIST_DIRECTORY, |_| None), None);
    }
}
