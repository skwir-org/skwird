//! How far an answer below a path can be trusted, from an index: every
//! folder from the root down to it read (one not covered above makes the
//! whole not covered), then its own subtree's proof and holes.

use std::path::{Path, PathBuf};

use skwird_index::Index;
use skwird_index::block::State;
use skwird_protocol as wire;

use crate::convert;

/// The coverage below `rel`, for an index of `root`; none if no folder of
/// the index is there.
pub fn coverage_at(index: &Index, root: &Path, rel: &Path) -> Option<wire::Coverage> {
    let not_covered = |why| wire::Coverage {
        names: wire::Proof::NotCovered(convert::why(why)),
        attrs: wire::Proof::NotCovered(convert::why(why)),
        holes: Vec::new(),
    };
    let mut above = PathBuf::new();
    let mut folder = index.folder_at(&above)?;
    for part in rel.components() {
        if let Some(State::NotCovered(why)) = index.state_of(folder) {
            return Some(not_covered(why));
        }
        above.push(part);
        folder = index.folder_at(&above)?;
    }
    if let Some(State::NotCovered(why)) = index.state_of(folder) {
        return Some(not_covered(why));
    }
    let c = index.coverage(folder);
    // Within a live subtree, names and attributes are both live: a change
    // to an entry is heard, and its folder read again, stat and all.
    let live = index
        .live_since(rel)
        .map(|t| wire::Proof::Live(convert::time(t)));
    Some(wire::Coverage {
        names: live.unwrap_or_else(|| convert::proof(c.names)),
        attrs: live.unwrap_or_else(|| wire::Proof::AsOf(convert::time(c.attrs))),
        holes: c
            .holes
            .into_iter()
            .map(|h| wire::Hole {
                path: wire::WirePath::of(&root.join(h.path)),
                why: convert::why(h.why),
            })
            .collect(),
    })
}

/// The weaker of two proofs, for an answer that holds both: not covered
/// before a snapshot, a snapshot before a check, a check before live. Of
/// two alike, the one that says least: the older snapshot or check, the
/// later start of live.
pub fn weaker(a: wire::Proof, b: wire::Proof) -> wire::Proof {
    use wire::Proof::{AsOf, Checked, Live, NotCovered};
    let rank = |p: &wire::Proof| match p {
        NotCovered(_) => 0,
        AsOf(_) => 1,
        Checked(_) => 2,
        Live(_) => 3,
    };
    match (a, b) {
        (Live(x), Live(y)) => Live(x.max(y)),
        (Checked(x), Checked(y)) => Checked(x.min(y)),
        (AsOf(x), AsOf(y)) => AsOf(x.min(y)),
        (a, b) if rank(&a) <= rank(&b) => a,
        (_, b) => b,
    }
}

/// `outer`, the coverage of a folder above another root, with that root's
/// coverage, `inner`, at its place: the mount hole there gone, its own
/// holes in, each proof the weaker of both.
pub fn with_root_below(
    mut outer: wire::Coverage,
    at: &Path,
    inner: wire::Coverage,
) -> wire::Coverage {
    let at = wire::WirePath::of(at);
    outer.holes.retain(|h| h.path != at);
    outer.holes.extend(inner.holes);
    outer.names = weaker(outer.names, inner.names);
    outer.attrs = weaker(outer.attrs, inner.attrs);
    outer
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(sec: i64) -> wire::Instant {
        wire::Instant { sec, nsec: 0 }
    }

    #[test]
    fn the_weaker_proof_says_least() {
        use wire::Proof::{AsOf, Checked, Live, NotCovered};
        assert_eq!(weaker(Live(at(5)), Live(at(9))), Live(at(9)));
        assert_eq!(weaker(Checked(at(5)), Checked(at(9))), Checked(at(5)));
        assert_eq!(weaker(AsOf(at(5)), Live(at(1))), AsOf(at(5)));
        assert_eq!(weaker(Live(at(1)), Checked(at(9))), Checked(at(9)));
        assert_eq!(
            weaker(Checked(at(1)), NotCovered(wire::Why::Unread)),
            NotCovered(wire::Why::Unread)
        );
    }
}
