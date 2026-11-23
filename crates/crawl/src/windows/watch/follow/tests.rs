use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use skwird_testkit::Scratch;

use super::super::{Heard, Watcher};

/// What `w` hears, batch after batch, read on a thread of its own.
fn hearing(w: &Arc<Watcher>) -> Receiver<Vec<Heard>> {
    let (send, receive) = channel();
    let w = Arc::clone(w);
    std::thread::spawn(move || {
        loop {
            let mut batch = Vec::new();
            if w.read(&mut batch).is_err() || send.send(batch).is_err() {
                return;
            }
        }
    });
    receive
}

/// What was heard until `done` holds of it, ten seconds at most.
fn until(heard: &Receiver<Vec<Heard>>, done: impl Fn(&[Heard]) -> bool) -> Vec<Heard> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut all = Vec::new();
    while !done(&all) {
        let left = deadline.saturating_duration_since(Instant::now());
        match heard.recv_timeout(left) {
            Ok(batch) => all.extend(batch),
            Err(e) => panic!("not heard in time ({e}); heard: {all:?}"),
        }
    }
    all
}

fn entries(heard: &[Heard], scope: usize) -> Vec<PathBuf> {
    heard
        .iter()
        .filter_map(|h| match h {
            Heard::Entry { scope: s, rel } if *s == scope => Some(rel.clone()),
            _ => None,
        })
        .collect()
}

/// A root followed through its volume's journal hears what is made,
/// written, renamed and removed below it, by its path below the root, and
/// nothing from outside it; a cookie made in the cookies' folder after
/// those changes is said after them. On a test machine's NTFS volume alone
/// (`skwird_testkit::ntfs`), as only an administrator may read a journal.
#[test]
fn a_journal_says_what_changed_below_its_root() {
    let Some(volume) = skwird_testkit::ntfs() else {
        return;
    };
    // As the system service runs: a folder denied the test's token
    // (`System Volume Information`, which Windows writes in at times) is
    // still opened, not a doubt.
    assert!(crate::read_anything(), "an elevated administrator holds it");
    let s = Scratch::under(&volume, "follow");
    let root = s.dir("tree");
    let cookies = s.dir(r"tree\cookies");
    s.file(r"tree\gone.txt");
    let w = Arc::new(Watcher::new(1, 0).unwrap());
    w.follow_root(4, &root).unwrap();
    w.watch_cookies(&cookies).unwrap();
    let heard = hearing(&w);
    s.write(r"tree\a b\été 2026.txt", b"x");
    s.write("outside.txt", b"x");
    std::fs::rename(
        s.join(r"tree\a b\été 2026.txt"),
        s.join(r"tree\a b\renamed.txt"),
    )
    .unwrap();
    std::fs::remove_file(s.join(r"tree\gone.txt")).unwrap();
    s.file(r"tree\cookies\cookie-9");
    let all = until(&heard, |h| h.contains(&Heard::Cookie(9)));
    let said = entries(&all, 4);
    for rel in [r"a b", r"a b\été 2026.txt", r"a b\renamed.txt", "gone.txt"] {
        assert!(said.contains(&PathBuf::from(rel)), "{rel}: {all:?}");
    }
    assert!(
        said.iter()
            .all(|r| !r.starts_with("..") && r != Path::new("outside.txt")),
        "nothing from outside the root: {all:?}"
    );
    let cookie = all.iter().position(|h| *h == Heard::Cookie(9)).unwrap();
    let renamed = all
        .iter()
        .position(|h| matches!(h, Heard::Entry { rel, .. } if rel == Path::new(r"a b\renamed.txt")))
        .unwrap();
    assert!(
        renamed < cookie,
        "the cookie after what was made before it: {all:?}"
    );
    assert!(
        !all.iter()
            .any(|h| matches!(h, Heard::Overflow { .. } | Heard::Lost { .. })),
        "{all:?}"
    );
}

/// A root followed through a journal reads nothing while nothing changes
/// (rule 6: the read waits in the system, never asked again and again),
/// and its watch ends once the journal is deleted, so that it is checked
/// from then on, never live. On the test machine's spare volume, which no
/// other test reads, as the journal is deleted.
#[test]
fn a_followed_root_is_idle_and_its_watch_ends_with_its_journal() {
    let Some(volume) = skwird_testkit::ntfs_spare() else {
        return;
    };
    let s = Scratch::under(&volume, "follow-ends");
    let root = s.dir("tree");
    let w = Arc::new(Watcher::new(1, 0).unwrap());
    w.follow_root(2, &root).unwrap();
    let heard = hearing(&w);
    s.write(r"tree\one.txt", b"x");
    until(&heard, |h| {
        entries(h, 2).contains(&PathBuf::from("one.txt"))
    });
    std::thread::sleep(Duration::from_millis(500));
    let before = w.journal_reads();
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        w.journal_reads() <= before + 1,
        "reads while nothing changed: {} then {}",
        before,
        w.journal_reads()
    );
    let letter = volume.to_str().unwrap().trim_end_matches('\\');
    let deleted = std::process::Command::new("fsutil")
        .args(["usn", "deletejournal", "/d", letter])
        .output()
        .unwrap();
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stdout)
    );
    let all = until(&heard, |h| h.contains(&Heard::Lost { scope: 2 }));
    assert!(all.contains(&Heard::Lost { scope: 2 }));
}

/// The volume's metadata is told by its record's number alone: the first
/// sixteen records but the root folder's, whatever its sequence number.
#[test]
fn the_volumes_metadata_is_told_by_its_number() {
    use super::is_metadata;
    let at = |number: u128, sequence: u128| number | (sequence << 48);
    assert!(is_metadata(at(11, 11)), "$Extend");
    assert!(is_metadata(at(0, 1)), "$MFT");
    assert!(!is_metadata(at(5, 5)), "the root folder");
    assert!(!is_metadata(at(16, 1)));
    assert!(!is_metadata(at(40, 2)));
}
