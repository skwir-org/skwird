//! A folder's totals answered (`docs/DESIGN.md`, Protocol, `totals`):
//! from the snapshot published when they are asked
//! (`skwird_index::totals`), each child of the folder in batches, then its
//! own totals and how far they hold.
//!
//! - **The journal's token first,** then the snapshot: the snapshot is at
//!   least as new as the token, so that `changes` since it holds every
//!   change the totals do not.
//! - **Per asker:** another user than root, at the system service, is
//!   counted what they could list alone. A folder they may not list is a
//!   hole, and nothing below it counts.
//! - **Holes** are the folders not counted, by path: the coverage's, as
//!   the asker may see them, and those the totals passed over. A folder
//!   another root holds (a mount) is one: asked there, it answers.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};

use skwird_index::block::FolderId;
use skwird_index::keeper::TooOld;
use skwird_index::{Index, Sight, totals};
use skwird_protocol::{self as wire, Answer, WirePath};

use crate::asker::Asker;
use crate::convert;
use crate::coverage::coverage_at;
use crate::engine::{Command, Published, Shared};
use crate::serve::{
    LINE_ROOM, Reached, Writer, batches, denied, failure, json_len, outside, reached, seeing, send,
};

/// What the folder at `root` holds, answered to `who`: its children sent
/// on `writer` as request `id`'s, so many with `largest`, the rest
/// summed; then the folder's own totals, given back. Cancelled, the rest
/// of the children are not sent, and the totals say so.
// What a query is given, and the writer's channel for the token.
#[allow(clippy::too_many_arguments)]
pub(crate) fn answer(
    shared: &Shared,
    commands: &Sender<Command>,
    writer: &Writer,
    id: u64,
    root: &WirePath,
    largest: Option<wire::Largest>,
    cancelled: &AtomicBool,
    who: &Asker,
) -> Answer {
    let path = root.to_path();
    let done = |token: String, coverage: wire::Coverage| wire::TotalsDone {
        mtime: None,
        totals: wire::Totals::default(),
        token,
        coverage,
        nlink: wire::Nlink::Complete,
        count: 0,
        rest: None,
        cancelled: cancelled.load(Ordering::Relaxed),
    };
    let Some((i, rel)) = shared.locate(&path) else {
        return Answer::TotalsDone(Box::new(done(String::new(), outside())));
    };
    let unread = || wire::Coverage {
        names: wire::Proof::NotCovered(wire::Why::Unread),
        attrs: wire::Proof::NotCovered(wire::Why::Unread),
        holes: Vec::new(),
    };
    let token = match token_now(commands, i, &rel) {
        Some(Ok(token)) => token,
        // No token to go on from: the root is being opened (a first crawl,
        // a kept volume's pause), checked on a thread of its own, or let go
        // of. Nothing of it is counted yet.
        Some(Err(TooOld)) => return Answer::TotalsDone(Box::new(done(String::new(), unread()))),
        None => return failure("internal", "the engine is going"),
    };
    let index = match shared.snapshot(i) {
        Published::Ready(index) => index,
        Published::Opening => return Answer::TotalsDone(Box::new(done(token, unread()))),
        Published::Failed(why) => return failure("internal", why),
    };
    let seeing = seeing(shared, i, &index, who);
    let mut memo = HashMap::new();
    if let Some(s) = &seeing
        && let Reached::Denied = reached(s, &rel, &mut memo)
    {
        return Answer::TotalsDone(Box::new(done(token, denied())));
    }
    let not_a_folder = || failure("not_a_folder", "no folder of the index is at that path");
    let base = shared.root(i);
    let Some(mut coverage) = coverage_at(&index, &base, &rel) else {
        return not_a_folder();
    };
    let Some(folder) = index.folder_at(&rel) else {
        return not_a_folder();
    };
    let found = match &seeing {
        None => index.totals(folder, None),
        Some(s) => {
            let sight = |f: FolderId, memo: &mut HashMap<FolderId, Sight>| s.sight(f, memo);
            index.totals(folder, Some(&sight))
        }
    };
    let Some(found) = found else {
        return not_a_folder();
    };
    // A hole is a folder's name: the coverage's said only to who may list
    // its parent; the totals' are below folders the asker may list.
    if let Some(s) = &seeing {
        s.seen_holes(&base, &mut coverage.holes, &mut memo);
    }
    add_holes(&mut coverage, &found, &index, &base);
    let children = children_of(&found, &base.join(&rel));
    let (sent, rest) = match largest {
        Some(l) => wire::largest(children, l),
        None => (children, None),
    };
    let mut count = 0;
    for (batch, fits) in batches(&sent) {
        if cancelled.load(Ordering::Relaxed) {
            break;
        }
        if !fits {
            return too_long();
        }
        if !send(writer, id, Answer::Children(batch.to_vec())) {
            break;
        }
        count += batch.len() as u64;
    }
    let done = Answer::TotalsDone(Box::new(wire::TotalsDone {
        mtime: found.mtime.map(convert::time),
        totals: totals_of(&found.own),
        nlink: if found.partial {
            wire::Nlink::Partial
        } else {
            wire::Nlink::Complete
        },
        count,
        rest,
        ..done(token, coverage)
    }));
    if json_len(&done) > LINE_ROOM {
        return too_long();
    }
    done
}

/// A folder's totals, or a child's, longer than a line a client reads:
/// `by_extension` names every extension below it, so that it sums to its
/// files' size, and a folder of mail (each name its own extension) holds
/// tens of thousands. Said so, not sent as a line the client refuses.
fn too_long() -> Answer {
    failure(
        "internal",
        "the totals are longer than a line may be: too many extensions below",
    )
}

/// The folders the totals passed over added to `coverage`'s holes, by
/// their paths below the root at `base`, each once.
fn add_holes(coverage: &mut wire::Coverage, found: &totals::Totals, index: &Index, base: &Path) {
    for (f, why) in &found.holes {
        let path = WirePath::of(&base.join(index.path_of(*f)));
        if !coverage.holes.iter().any(|h| h.path == path) {
            coverage.holes.push(wire::Hole {
                path,
                why: convert::why(*why),
            });
        }
    }
}

/// The children found, as the protocol says them, below the folder at
/// `at`.
fn children_of(found: &totals::Totals, at: &Path) -> Vec<wire::Child> {
    found
        .children
        .iter()
        .map(|c| wire::Child {
            path: WirePath::of(&at.join(skwird_index::index::component(&c.name))),
            kind: convert::kind(c.kind),
            id: convert::id(c.id),
            mtime: c.mtime.map(convert::time),
            totals: totals_of(&c.sum),
        })
        .collect()
}

/// A subtree's sums, as the protocol says them: by extension, the largest
/// alloc first.
fn totals_of(sum: &totals::Sum) -> wire::Totals {
    let mut by_extension: Vec<wire::Extension> = sum
        .by_extension
        .iter()
        .map(|(ext, &(files, alloc))| wire::Extension {
            ext: ext.clone().map(WirePath),
            files,
            alloc,
        })
        .collect();
    by_extension.sort_by(|a, b| b.alloc.cmp(&a.alloc).then_with(|| a.ext.cmp(&b.ext)));
    wire::Totals {
        size: sum.size,
        alloc: sum.alloc,
        files: sum.files,
        folders: sum.folders,
        names: sum.names,
        newest_file_mtime: sum.newest.map(convert::time),
        by_extension,
        shared: sum.shared,
    }
}

/// Root `i`'s journal token now, at `rel`: a `changes` from nothing, which
/// the writer answers with no event and the token to go on from, or with
/// none where it holds no index of the root now. None where the engine is
/// going.
fn token_now(commands: &Sender<Command>, i: usize, rel: &Path) -> Option<Result<String, TooOld>> {
    let (reply, answer) = channel();
    commands
        .send(Command::Changes {
            root: i,
            rel: rel.to_path_buf(),
            since: String::new(),
            reply,
        })
        .ok()?;
    Some(answer.recv().ok()?.map(|(_, token, _)| token.text()))
}
