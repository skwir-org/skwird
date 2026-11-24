//! Removable volumes kept by clients (`docs/DESIGN.md`, Protocol,
//! "Removable volumes"; open question 8): what follows from what each
//! client keeps, what is mounted, and what was released, as plain data the
//! writer applies (`engine::Writing::volumes_followed`).
//!
//! - **A kept volume, mounted, is a root,** at its mount point, its index
//!   in the engine's cache by its identity, never on the volume; not
//!   mounted, it is no root, its index kept.
//! - **Two mounted of one identity** (a stick copied whole): the one a
//!   root already stays one, or the first in the mount table's order; the
//!   other is a twin, refused.
//! - **Released** before an eject: no root, until it is seen unmounted, or
//!   [`RELEASED_FOR`] has passed, should the unmount have been refused.
//! - **The system service keeps none:** a user's engine's alone.

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use skwird_crawl::removable::Removable;
use skwird_protocol::{KeptVolume, SeenVolume, Standing, WirePath};

use crate::dirs::Dirs;

/// How long a released volume is read no more, if it is not seen
/// unmounted: an unmount refused (a program holding a file there) leaves
/// it mounted, and it is a root again then.
pub const RELEASED_FOR: Duration = Duration::from_secs(60);

/// How long after a kept volume is seen mounted it is first read: the copy
/// the user plugged it in for comes first.
pub const PAUSE: Duration = Duration::from_secs(3);

/// How long a release waits for the reads of a volume to end before it is
/// answered all the same.
pub const RELEASE_WAIT: Duration = Duration::from_secs(5);

/// The reads in progress on a kept volume, which a release waits to end
/// before it is answered: the client unmounts it as it is.
#[derive(Debug, Default)]
pub struct Reading {
    count: Mutex<usize>,
    ended: Condvar,
}

/// A read in progress, ended when dropped.
pub struct Read(Arc<Reading>);

impl Reading {
    /// A read begun: begun by the writer before the thread that reads is
    /// made, so that a release met before the thread runs waits for it.
    pub fn begin(self: &Arc<Self>) -> Read {
        *self.count.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        Read(Arc::clone(self))
    }

    /// Whether every read ended within `bound`.
    pub fn ended_within(&self, bound: Duration) -> bool {
        let count = self.count.lock().unwrap_or_else(PoisonError::into_inner);
        self.ended
            .wait_timeout_while(count, bound, |n| *n > 0)
            .is_ok_and(|(n, _)| *n == 0)
    }
}

impl Drop for Read {
    fn drop(&mut self) {
        let mut count = self.0.count.lock().unwrap_or_else(PoisonError::into_inner);
        *count = count.saturating_sub(1);
        self.0.ended.notify_all();
    }
}

/// A removable volume a client keeps, kept across restarts in the state
/// folder, as exclusions are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Kept {
    pub client: String,
    pub id: String,
}

/// The file of what clients keep, in the state folder.
pub const FILE: &str = "volumes";

/// `forget` taken back, then `keep` kept, for `client`, each identity once
/// a client: whether anything changed.
pub fn changed(all: &mut Vec<Kept>, client: &str, keep: &[String], forget: &[String]) -> bool {
    let before = all.clone();
    all.retain(|k| !(k.client == client && forget.contains(&k.id)));
    for id in keep {
        if !all.iter().any(|k| k.client == client && k.id == *id) {
            all.push(Kept {
                client: client.to_owned(),
                id: id.clone(),
            });
        }
    }
    *all != before
}

/// Whether any client keeps `id`.
pub fn is_kept(all: &[Kept], id: &str) -> bool {
    all.iter().any(|k| k.id == id)
}

/// What the writer holds of removable volumes.
#[derive(Debug, Default)]
pub struct State {
    /// What clients keep, in the order kept.
    pub kept: Vec<Kept>,
    /// Those mounted, as last seen: none where the system's are not seen.
    pub seen: Vec<Removable>,
    /// Those released before an eject, each until when.
    pub released: HashMap<String, Instant>,
    /// What they ask of the roots, as last followed.
    pub plan: Plan,
}

impl State {
    /// What clients keep, none seen yet.
    pub fn kept(kept: Vec<Kept>) -> State {
        State {
            kept,
            ..State::default()
        }
    }
}

/// What the kept, the mounted and the released ask of the roots.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Each kept volume to be a root: its identity and its mount point.
    pub roots: Vec<(String, PathBuf)>,
    /// Each kept volume mounted and refused, a twin of one that is a root.
    pub twins: Vec<(String, PathBuf)>,
}

/// The roots `kept`, `seen` and `released` (each identity until when) ask
/// for at `now`, `rooted` the volumes roots already (identity, mount
/// point).
pub fn plan<S: BuildHasher, R: BuildHasher>(
    kept: &[Kept],
    seen: &[Removable],
    released: &HashMap<String, Instant, S>,
    now: Instant,
    rooted: &HashMap<String, PathBuf, R>,
) -> Plan {
    let mut plan = Plan::default();
    // Each identity once, in the order its first mount is in the table.
    let mut ids: Vec<&str> = Vec::new();
    for id in seen.iter().filter_map(|s| s.id.as_deref()) {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    for id in ids {
        if !is_kept(kept, id) || released.get(id).is_some_and(|until| *until > now) {
            continue;
        }
        let points: Vec<&PathBuf> = seen
            .iter()
            .filter(|s| s.id.as_deref() == Some(id))
            .map(|s| &s.point)
            .collect();
        // The one a root already stays one; else the first.
        let root = rooted
            .get(id)
            .filter(|at| points.contains(at))
            .unwrap_or(points[0]);
        plan.roots.push((id.to_owned(), root.clone()));
        for p in points.into_iter().filter(|p| *p != root) {
            plan.twins.push((id.to_owned(), p.clone()));
        }
    }
    plan
}

/// What a client is answered: each volume it keeps, as it stands, and
/// each mounted, kept by any client or not. Coverage is the serving
/// thread's to add, from what is published.
pub fn answer<S: BuildHasher>(
    all: &[Kept],
    client: &str,
    seen: &[Removable],
    plan: &Plan,
    released: &HashMap<String, Instant, S>,
    now: Instant,
) -> skwird_protocol::Volumes {
    let mounted = |id: &str| seen.iter().find(|s| s.id.as_deref() == Some(id));
    let kept = all
        .iter()
        .filter(|k| k.client == client)
        .map(|k| {
            let root = plan.roots.iter().find(|(id, _)| *id == k.id);
            let twin = plan.twins.iter().find(|(id, _)| *id == k.id);
            let (standing, at) = match (root, twin, mounted(&k.id)) {
                (Some((_, at)), _, _) => (Standing::Indexed, Some(at.as_path())),
                (None, Some((_, at)), _) => (Standing::Twin, Some(at.as_path())),
                (None, None, Some(s)) if released.get(&k.id).is_some_and(|until| *until > now) => {
                    (Standing::Released, Some(s.point.as_path()))
                }
                _ => (Standing::Away, None),
            };
            KeptVolume {
                id: k.id.clone(),
                standing,
                at: at.map(WirePath::of),
                coverage: None,
            }
        })
        .collect();
    let seen = seen
        .iter()
        .map(|s| SeenVolume {
            id: s.id.clone(),
            at: WirePath::of(&s.point),
            filesystem: s.fstype.clone(),
            label: s.label.clone(),
            kept: s.id.as_deref().is_some_and(|id| is_kept(all, id)),
        })
        .collect();
    skwird_protocol::Volumes { kept, seen }
}

/// Where a kept volume's index is: in the engine's cache, named by its
/// identity's hash, never on the volume.
pub fn index_dir(dirs: &Dirs, id: &str) -> PathBuf {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in id.as_bytes() {
        hash = (hash ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
    }
    dirs.cache.join(format!("volume-{hash:016x}"))
}

/// Whether `path` is where a volume of `seen` is mounted.
pub fn is_mounted_at(seen: &[Removable], path: &Path) -> bool {
    seen.iter().any(|s| s.point == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seen(id: Option<&str>, point: &str) -> Removable {
        Removable {
            id: id.map(str::to_owned),
            point: point.into(),
            fstype: "vfat".into(),
            label: None,
        }
    }

    fn kept(client: &str, id: &str) -> Kept {
        Kept {
            client: client.into(),
            id: id.into(),
        }
    }

    /// A client keeps and forgets each identity once; another's are its
    /// own.
    #[test]
    fn a_client_keeps_and_forgets_its_own() {
        let mut all = vec![kept("skwir", "AAAA-0001")];
        assert!(changed(&mut all, "skwir", &["AAAA-0002".into()], &[]));
        assert!(!changed(&mut all, "skwir", &["AAAA-0002".into()], &[]));
        assert!(changed(&mut all, "other", &["AAAA-0001".into()], &[]));
        assert!(changed(&mut all, "skwir", &[], &["AAAA-0001".into()]));
        assert_eq!(
            all,
            [kept("skwir", "AAAA-0002"), kept("other", "AAAA-0001")]
        );
        assert!(is_kept(&all, "AAAA-0001"));
    }

    /// A kept volume mounted is a root; one not kept, one with no
    /// identity, and one released are not; a second of one identity is a
    /// twin, the one a root already staying one.
    #[test]
    fn the_roots_are_the_kept_volumes_mounted() {
        let all = [
            kept("skwir", "AAAA-0001"),
            kept("skwir", "AAAA-0002"),
            kept("skwir", "AAAA-0003"),
        ];
        let mounted = [
            seen(Some("AAAA-0001"), "/media/one"),
            seen(Some("BBBB-0001"), "/media/not kept"),
            seen(None, "/media/no identity"),
            seen(Some("AAAA-0002"), "/media/copy"),
            seen(Some("AAAA-0002"), "/media/two"),
            seen(Some("AAAA-0003"), "/media/released"),
        ];
        let now = Instant::now();
        let released = HashMap::from([("AAAA-0003".to_owned(), now + RELEASED_FOR)]);
        let rooted = HashMap::from([("AAAA-0002".to_owned(), PathBuf::from("/media/two"))]);
        let p = plan(&all, &mounted, &released, now, &rooted);
        assert_eq!(
            p.roots,
            [
                ("AAAA-0001".to_owned(), PathBuf::from("/media/one")),
                ("AAAA-0002".to_owned(), PathBuf::from("/media/two")),
            ]
        );
        assert_eq!(
            p.twins,
            [("AAAA-0002".to_owned(), PathBuf::from("/media/copy"))]
        );
        // Released a while ago, it is a root again.
        let later = now + RELEASED_FOR + Duration::from_secs(1);
        let p = plan(&all, &mounted, &released, later, &rooted);
        assert!(p.roots.iter().any(|(id, _)| id == "AAAA-0003"));
        // Not a root yet, the first in the table's order is the one.
        let p = plan(&all, &mounted, &released, now, &HashMap::new());
        assert!(
            p.roots
                .contains(&("AAAA-0002".to_owned(), PathBuf::from("/media/copy")))
        );
        assert_eq!(
            p.twins,
            [("AAAA-0002".to_owned(), PathBuf::from("/media/two"))]
        );
    }

    /// A client is answered its own volumes, each as it stands, and every
    /// volume mounted.
    #[test]
    fn a_client_is_answered_its_volumes_as_they_stand() {
        let all = [
            kept("skwir", "AAAA-0001"),
            kept("skwir", "AAAA-0002"),
            kept("skwir", "AAAA-0003"),
            kept("skwir", "AAAA-0004"),
            kept("other", "CCCC-0001"),
        ];
        let mounted = [
            seen(Some("AAAA-0001"), "/media/one"),
            seen(Some("AAAA-0002"), "/media/copy"),
            seen(Some("AAAA-0002"), "/media/two"),
            seen(Some("AAAA-0003"), "/media/released"),
            seen(Some("BBBB-0001"), "/media/not kept"),
        ];
        let now = Instant::now();
        let released = HashMap::from([("AAAA-0003".to_owned(), now + RELEASED_FOR)]);
        let p = plan(&all, &mounted, &released, now, &HashMap::new());
        let a = answer(&all, "skwir", &mounted, &p, &released, now);
        let standings: Vec<(&str, Standing, Option<&WirePath>)> = a
            .kept
            .iter()
            .map(|k| (k.id.as_str(), k.standing, k.at.as_ref()))
            .collect();
        let at = |p: &str| WirePath::of(Path::new(p));
        let (one, copy, two, rel) = (
            at("/media/one"),
            at("/media/copy"),
            at("/media/two"),
            at("/media/released"),
        );
        assert_eq!(
            standings,
            [
                ("AAAA-0001", Standing::Indexed, Some(&one)),
                ("AAAA-0002", Standing::Indexed, Some(&copy)),
                ("AAAA-0003", Standing::Released, Some(&rel)),
                ("AAAA-0004", Standing::Away, None),
            ]
        );
        let _ = two;
        let kept_seen: Vec<(Option<&str>, bool)> =
            a.seen.iter().map(|s| (s.id.as_deref(), s.kept)).collect();
        assert_eq!(
            kept_seen,
            [
                (Some("AAAA-0001"), true),
                (Some("AAAA-0002"), true),
                (Some("AAAA-0002"), true),
                (Some("AAAA-0003"), true),
                (Some("BBBB-0001"), false),
            ]
        );
    }
}
