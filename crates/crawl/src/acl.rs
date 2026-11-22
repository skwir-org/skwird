//! A folder's access ACL (`docs/DESIGN.md`, decision 4; phase 3, step
//! 5b): read as Linux keeps it (`system.posix_acl_access`), and evaluated
//! as the kernel evaluates it (acl(5)), so that the system service answers
//! each asker what a folder with an ACL lets them list.

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
pub struct Acl {
    entries: Vec<(u16, u16, u32)>,
}

impl Acl {
    /// An ACL as the kernel writes it in `system.posix_acl_access`: a
    /// version, then entries of a tag, permissions and an id. None if it
    /// is not one, or lacks an entry every ACL has.
    pub fn parse(bytes: &[u8]) -> Option<Acl> {
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
        (has(USER_OBJ) && has(GROUP_OBJ) && has(OTHER)).then_some(Acl { entries })
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

#[cfg(test)]
mod tests {
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
}
