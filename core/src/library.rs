//! The user's library: the folders Filefind indexes.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "StoredFolder")]
pub struct Folder {
    pub path: PathBuf,
    pub include_subfolders: bool,
}

/// Accepts both the current format and plain paths written by version 0.1.
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredFolder {
    Path(PathBuf),
    Full { path: PathBuf, include_subfolders: bool },
}

impl From<StoredFolder> for Folder {
    fn from(stored: StoredFolder) -> Folder {
        match stored {
            StoredFolder::Path(path) => Folder::new(path),
            StoredFolder::Full { path, include_subfolders } => Folder { path, include_subfolders },
        }
    }
}

impl Folder {
    pub fn new(path: PathBuf) -> Folder {
        Folder { path, include_subfolders: true }
    }

    /// Whether files directly inside `dir` belong to this folder.
    pub fn covers_dir(&self, dir: &Path) -> bool {
        if self.include_subfolders {
            dir.starts_with(&self.path)
        } else {
            dir == self.path
        }
    }
}

/// Reads JSON written by [`save_json`], falling back to defaults if it is missing or invalid.
pub fn load_json<T: serde::de::DeserializeOwned + Default>(file: &Path) -> T {
    std::fs::read(file).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

/// Writes `value` as JSON atomically, so a crash can never leave a half-written file.
pub fn save_json<T: Serialize>(file: &Path, value: &T) -> io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(value)?)?;
    std::fs::rename(tmp, file)
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Library {
    pub folders: Vec<Folder>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    /// The folder is already searched, directly or through a parent folder.
    AlreadyIncluded,
    /// Added, replacing this many folders that were inside it.
    Merged(usize),
}

impl Library {
    pub fn load(file: &Path) -> Library {
        load_json(file)
    }

    pub fn save(&self, file: &Path) -> io::Result<()> {
        save_json(file, self)
    }

    pub fn add(&mut self, path: PathBuf) -> AddOutcome {
        let covered = self.folders.iter().any(|f| f.path == path || (f.include_subfolders && path.starts_with(&f.path)));
        if covered {
            return AddOutcome::AlreadyIncluded;
        }
        let merged = self.absorb_children(&path);
        self.folders.push(Folder::new(path));
        self.folders.sort_by(|a, b| a.path.cmp(&b.path));
        if merged > 0 {
            AddOutcome::Merged(merged)
        } else {
            AddOutcome::Added
        }
    }

    /// Returns how many folders inside `path` were merged into it.
    pub fn set_include_subfolders(&mut self, path: &Path, include: bool) -> usize {
        let merged = if include { self.absorb_children(path) } else { 0 };
        if let Some(folder) = self.folders.iter_mut().find(|f| f.path == path) {
            folder.include_subfolders = include;
        }
        merged
    }

    pub fn remove(&mut self, path: &Path) {
        self.folders.retain(|f| f.path != path);
    }

    /// Removes folders strictly inside `path`, which a recursive `path` already covers.
    fn absorb_children(&mut self, path: &Path) -> usize {
        let before = self.folders.len();
        self.folders.retain(|f| f.path == path || !f.path.starts_with(path));
        before - self.folders.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(lib: &Library) -> Vec<&str> {
        lib.folders.iter().map(|f| f.path.to_str().unwrap()).collect()
    }

    #[test]
    fn add_nested() {
        let mut lib = Library::default();
        assert_eq!(lib.add("/home/u/Documents/Work".into()), AddOutcome::Added);
        assert_eq!(lib.add("/home/u/Documents/Taxes".into()), AddOutcome::Added);
        assert_eq!(lib.add("/home/u/Documents/Work/2024".into()), AddOutcome::AlreadyIncluded);
        assert_eq!(lib.add("/home/u/Documents".into()), AddOutcome::Merged(2));
        assert_eq!(paths(&lib), ["/home/u/Documents"]);
        // A sibling that merely shares a name prefix is not nested.
        assert_eq!(lib.add("/home/u/Documents2".into()), AddOutcome::Added);
    }

    #[test]
    fn folder_only() {
        let mut lib = Library::default();
        lib.add("/home/u".into());
        assert_eq!(lib.set_include_subfolders(Path::new("/home/u"), false), 0);
        // Subfolders of a "this folder only" entry can be added on their own.
        assert_eq!(lib.add("/home/u/Documents".into()), AddOutcome::Added);
        assert_eq!(lib.add("/home/u".into()), AddOutcome::AlreadyIncluded);
        // Including subfolders again absorbs them.
        assert_eq!(lib.set_include_subfolders(Path::new("/home/u"), true), 1);
        assert_eq!(paths(&lib), ["/home/u"]);

        let home = Folder { path: "/home/u".into(), include_subfolders: false };
        assert!(home.covers_dir(Path::new("/home/u")));
        assert!(!home.covers_dir(Path::new("/home/u/Documents")));
    }

    #[test]
    fn reads_old_format() {
        let lib: Library = serde_json::from_str(r#"{"folders":["/a",{"path":"/b","include_subfolders":false}]}"#).unwrap();
        assert_eq!(lib.folders, [Folder::new("/a".into()), Folder { path: "/b".into(), include_subfolders: false }]);
    }
}
