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
