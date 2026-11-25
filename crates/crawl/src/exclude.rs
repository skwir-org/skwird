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
//!   refused, never taken to mean everything;
//! - where volumes ignore case (Windows, macOS) and, on macOS, the form a
//!   name is written in, so does the path's comparison: a folder made
//!   later as `Secret` is excluded by `secret`. On a volume that tells
//!   case, a folder differing from an excluded one by its case alone is
//!   excluded too: never one less.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use crate::FileId;

/// The folders kept out of the index unless the user's configuration
/// says otherwise, below the user's home: keys and keyrings, and the
/// password store, whose names are the accounts it holds.
#[cfg(not(any(windows, target_os = "macos")))]
pub const DEFAULTS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".local/share/keyrings",
    ".local/share/kwalletd",
    ".password-store",
];

/// The same on macOS: `OpenSSH`'s, `GnuPG`'s and `pass`'s as on Linux, and
/// the user's keychains, where Linux has its keyrings.
#[cfg(target_os = "macos")]
pub const DEFAULTS: &[&str] = &[".ssh", ".gnupg", ".password-store", "Library/Keychains"];

/// The same on Windows, where they are kept elsewhere: OpenSSH's and
/// `pass`'s as on Unix, `GnuPG`'s in the roaming application data, and
/// Windows' own credentials and the keys that protect them (its
/// credential manager, its vault, its data protection keys).
#[cfg(windows)]
pub const DEFAULTS: &[&str] = &[
    ".ssh",
    ".password-store",
    "AppData/Roaming/gnupg",
    "AppData/Roaming/Microsoft/Credentials",
    "AppData/Local/Microsoft/Credentials",
    "AppData/Roaming/Microsoft/Protect",
    "AppData/Roaming/Microsoft/Crypto",
    "AppData/Local/Microsoft/Vault",
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
    /// Each path folded, where volumes ignore case, and the last names
    /// folded, so that a folder's whole path is folded only where its own
    /// name could be one.
    #[cfg(any(windows, target_os = "macos"))]
    folded: HashSet<String>,
    #[cfg(any(windows, target_os = "macos"))]
    folded_names: HashSet<String>,
}

/// A path as a volume that ignores case compares it, or more alike: as
/// uppercased (NTFS's upcase table, APFS' folding take ſ as s, and a final
/// ς as σ, which `to_lowercase` alone writes apart) then lowercased, ς as
/// σ, and on macOS composed, as APFS compares a name whatever form it is
/// written in. A name it takes as another's, a volume would not (ß as ss),
/// keeps out one folder more, never one less.
#[cfg(any(windows, target_os = "macos"))]
fn fold(path: &std::ffi::OsStr) -> String {
    let lower = path
        .to_string_lossy()
        .to_uppercase()
        .to_lowercase()
        .replace('\u{3c2}', "\u{3c3}");
    #[cfg(target_os = "macos")]
    let lower = {
        use unicode_normalization::UnicodeNormalization;
        lower.nfc().collect()
    };
    lower
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
                Ok((resolved, id)) => exclusions.keep(resolved, id),
                Err(why) => refused.push((path.to_path_buf(), why)),
            }
        }
        (exclusions, refused)
    }

    /// Exclude `path` too, resolved as [`Exclusions::resolve`] resolves
    /// it, if `keep` says so of the folder it resolves to: its resolved
    /// path, and its identity if a folder is there. Says whether it was
    /// kept. The system service keeps a user's exclusions so, each only
    /// if that user owns the folder (DESIGN 5).
    pub fn insert_if(
        &mut self,
        path: &Path,
        keep: impl FnOnce(&Path, Option<FileId>) -> bool,
    ) -> Result<bool, Refused> {
        let (resolved, id) = resolve_one(path)?;
        if !keep(&resolved, id) {
            return Ok(false);
        }
        self.keep(resolved, id);
        Ok(true)
    }

    fn keep(&mut self, resolved: PathBuf, id: Option<FileId>) {
        #[cfg(any(windows, target_os = "macos"))]
        {
            self.folded.insert(fold(resolved.as_os_str()));
            if let Some(name) = resolved.file_name() {
                self.folded_names.insert(fold(name));
            }
        }
        self.by_path.insert(resolved);
        if let Some(id) = id {
            self.by_id.insert(id);
        }
    }

    /// Whether the folder with this identity, at this resolved path, is
    /// excluded.
    pub fn holds(&self, id: Option<FileId>, path: &Path) -> bool {
        id.is_some_and(|id| self.by_id.contains(&id))
            || self.by_path.contains(path)
            || self.holds_folded(path)
    }

    #[cfg(any(windows, target_os = "macos"))]
    fn holds_folded(&self, path: &Path) -> bool {
        let Some(name) = path.file_name() else {
            return false;
        };
        let named = match name.to_str() {
            // An ASCII name folds to its ASCII lowercase, which no name
            // folded holding more than ASCII equals: compared as it is,
            // nothing made for it, as nearly every folder read is.
            Some(ascii) if ascii.is_ascii() => self
                .folded_names
                .iter()
                .any(|folded| folded.eq_ignore_ascii_case(ascii)),
            _ => !self.folded_names.is_empty() && self.folded_names.contains(&fold(name)),
        };
        named && self.folded.contains(&fold(path.as_os_str()))
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[allow(clippy::unused_self)] // the same call as on the systems that fold
    fn holds_folded(&self, _: &Path) -> bool {
        false
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
        let Ok(resolved) = crate::canonical(&existing) else {
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
    // As the crawl's identities keep it, or the two never compare equal.
    #[cfg(target_os = "macos")]
    let dev = crate::macos::dev_of(&meta);
    #[cfg(not(target_os = "macos"))]
    let dev = meta.dev();
    meta.is_dir().then(|| FileId {
        dev,
        ino: meta.ino(),
    })
}

#[cfg(windows)]
fn folder_id(path: &Path) -> Option<FileId> {
    crate::windows::folder_id(path)
}

#[cfg(not(any(unix, windows)))]
fn folder_id(_: &Path) -> Option<FileId> {
    None
}

/// Where volumes ignore case, a folder made later with another case, or
/// on macOS in another form, is excluded; on Linux it is another folder.
#[cfg(test)]
#[test]
fn an_exclusion_holds_whatever_the_case_of_the_folder_made_later() {
    let s = skwird_testkit::Scratch::new("exclude-case");
    let base = crate::canonical(s.path()).unwrap();
    let (ex, refused) = Exclusions::resolve([base.join("secret").join("Café")]);
    assert!(refused.is_empty(), "{refused:?}");
    // macOS composes too; Windows compares no form.
    let name = if cfg!(target_os = "macos") {
        "CAFE\u{301}"
    } else {
        "CAF\u{c9}"
    };
    let later = base.join("Secret").join(name);
    assert_eq!(
        ex.holds(None, &later),
        cfg!(windows) || cfg!(target_os = "macos")
    );
    assert!(!ex.holds(None, &base.join("Secret")));
    // A name of ASCII alone, compared as it is; one beyond it, folded.
    let (ex, _) = Exclusions::resolve([base.join("Keys"), base.join("\u{c9}t\u{e9}")]);
    let folds = cfg!(windows) || cfg!(target_os = "macos");
    assert_eq!(ex.holds(None, &base.join("KEYS")), folds);
    assert_eq!(ex.holds(None, &base.join("\u{c9}T\u{c9}")), folds);
    assert!(!ex.holds(None, &base.join("Keyring")));
    assert!(!ex.holds(None, &base.join("other").join("keys")));
    // A final sigma, a long s: as the volumes take them.
    let (ex, _) = Exclusions::resolve([
        base.join("\u{391}\u{3a1}\u{3a7}\u{395}\u{399}\u{39f}\u{3a3}"),
        base.join("secret"),
    ]);
    assert_eq!(
        ex.holds(
            None,
            &base.join("\u{3b1}\u{3c1}\u{3c7}\u{3b5}\u{3b9}\u{3bf}\u{3c3}")
        ),
        folds
    );
    assert_eq!(ex.holds(None, &base.join("\u{17f}ecret")), folds);
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
