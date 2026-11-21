//! Exclusions: folders never read into the index (`docs/DESIGN.md`,
//! decision 5).
//!
//! An exclusion is resolved once to the folder it names, and compared as
//! that folder, never as the spelling of a path. In fsearch an exclusion
//! written with a trailing slash excluded nothing, an empty one excluded
//! everything, and `/h/m` was taken to hold `/h/me`. Here:
//!
//! - the path is named as the system resolves it: links in it followed,
//!   doubled and trailing slashes dropped;
//! - the folder there is kept by its identity, and its resolved path is
//!   compared by whole components;
//! - a path that names no folder yet is kept by its resolved path, and
//!   applies to the folder made there;
//! - an empty or relative path, or one whose missing part holds `..`, is
//!   refused, never taken to mean everything.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::FileId;

/// The folders kept out of the index unless the user's configuration
/// says otherwise, below the user's home: keys and keyrings, and the
/// password store, whose names are the accounts it holds.
pub const DEFAULTS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".local/share/keyrings",
    ".local/share/kwalletd",
    ".password-store",
];

/// The default exclusions for a user whose home is `home`.
pub fn defaults(home: &Path) -> Vec<PathBuf> {
    DEFAULTS.iter().map(|rel| home.join(rel)).collect()
}

/// Why a path was refused as an exclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    Empty,
    Relative,
    /// Its part that does not exist yet holds `..`.
    Parent,
}

/// Folders kept out of the index.
#[derive(Clone, Debug, Default)]
pub struct Exclusions {
    by_id: HashSet<FileId>,
    by_path: HashSet<PathBuf>,
}

impl Exclusions {
    /// No exclusion at all.
    pub fn none() -> Exclusions {
        Exclusions::default()
    }

    /// Each path resolved; what cannot be is given back with the reason.
    pub fn resolve<P: AsRef<Path>>(
        paths: impl IntoIterator<Item = P>,
    ) -> (Exclusions, Vec<(PathBuf, Refused)>) {
        let mut exclusions = Exclusions::none();
        let mut refused = Vec::new();
        for path in paths {
            let path = path.as_ref();
            match resolve_one(path) {
                Ok((resolved, id)) => {
                    exclusions.by_path.insert(resolved);
                    if let Some(id) = id {
                        exclusions.by_id.insert(id);
                    }
                }
                Err(why) => refused.push((path.to_path_buf(), why)),
            }
        }
        (exclusions, refused)
    }

    /// Whether the folder with this identity, at this resolved path, is
    /// excluded.
    pub fn holds(&self, id: Option<FileId>, path: &Path) -> bool {
        id.is_some_and(|id| self.by_id.contains(&id)) || self.by_path.contains(path)
    }

    pub fn is_empty(&self) -> bool {
        self.by_path.is_empty()
    }

    /// Every folder excluded, by its resolved path.
    pub fn paths(&self) -> impl Iterator<Item = &Path> {
        self.by_path.iter().map(PathBuf::as_path)
    }
}

/// A path named as the system resolves it, and the identity of the folder
/// there, if there is one.
fn resolve_one(path: &Path) -> Result<(PathBuf, Option<FileId>), Refused> {
    if path.as_os_str().is_empty() {
        return Err(Refused::Empty);
    }
    if !path.is_absolute() {
        return Err(Refused::Relative);
    }
    // The longest part of it that exists, resolved by the system; the rest
    // as written, by its components.
    let components: Vec<Component<'_>> = path.components().collect();
    for split in (1..=components.len()).rev() {
        let existing: PathBuf = components[..split].iter().collect();
        let Ok(resolved) = std::fs::canonicalize(&existing) else {
            continue;
        };
        let mut full = resolved;
        for c in &components[split..] {
            match c {
                Component::Normal(name) => full.push(name),
                Component::CurDir => {}
                _ => return Err(Refused::Parent),
            }
        }
        let id = if split == components.len() {
            folder_id(&full)
        } else {
            None
        };
        return Ok((full, id));
    }
    Err(Refused::Relative)
}

#[cfg(unix)]
fn folder_id(path: &Path) -> Option<FileId> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).ok()?;
    meta.is_dir().then(|| FileId {
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

#[cfg(not(unix))]
fn folder_id(_: &Path) -> Option<FileId> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use skwird_testkit::Scratch;

    /// However the exclusion is written (a trailing slash, a doubled
    /// slash, through a link), it excludes its folder, and only it: not a
    /// folder whose name it begins, not the folder above.
    #[test]
    fn an_exclusion_names_its_folder_however_it_is_written() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new("exclude-spellings");
        s.dir("secret");
        s.dir("secret2");
        s.link("to-secret", "secret");
        let id = |rel: &str| {
            let meta = std::fs::metadata(s.join(rel)).unwrap();
            Some(FileId {
                dev: meta.dev(),
                ino: meta.ino(),
            })
        };
        let root = s.path().to_str().unwrap();
        for written in [
            format!("{root}/secret/"),
            format!("{root}//secret"),
            format!("{root}/./secret"),
            format!("{root}/to-secret"),
        ] {
            let (ex, refused) = Exclusions::resolve([&written]);
            assert!(refused.is_empty(), "{written}");
            assert!(ex.holds(id("secret"), &s.join("secret")), "{written}");
            assert!(!ex.holds(id("secret2"), &s.join("secret2")), "{written}");
            assert!(!ex.holds(None, s.path()), "{written}");
            assert!(
                !ex.holds(None, &s.join("to-secret")),
                "the link is not its folder"
            );
        }
    }

    /// A folder that does not exist yet is excluded once made there; a
    /// path that cannot name a folder is refused, never taken for all.
    #[test]
    fn an_exclusion_holds_before_its_folder_exists_and_nonsense_is_refused() {
        let s = Scratch::new("exclude-later");
        let later = s.join("later/inner");
        let (ex, refused) = Exclusions::resolve([&later]);
        assert!(refused.is_empty(), "{refused:?}");
        assert!(ex.holds(None, &later));
        assert!(!ex.holds(None, &s.join("later")));
        let (ex, refused) = Exclusions::resolve(["", "relative/path"]);
        assert!(ex.is_empty());
        assert_eq!(
            refused.iter().map(|r| r.1).collect::<Vec<_>>(),
            [Refused::Empty, Refused::Relative]
        );
        let missing = format!("{}/missing/../x", s.path().to_str().unwrap());
        let (_, refused) = Exclusions::resolve([missing]);
        assert_eq!(refused[0].1, Refused::Parent);
    }
}
