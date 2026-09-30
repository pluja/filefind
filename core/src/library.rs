//! The user's library: the set of folders Filefind indexes.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Library {
    pub folders: Vec<PathBuf>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AddOutcome {
    Added,
    /// The folder is already part of the library, directly or through a parent folder.
    AlreadyIncluded,
    /// Added, replacing this many folders that were inside it.
    Merged(usize),
}

impl Library {
    pub fn load(file: &Path) -> Library {
        std::fs::read(file)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Saves atomically, so a crash can never leave a half-written library.
    pub fn save(&self, file: &Path) -> io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(tmp, file)
    }

    pub fn add(&mut self, folder: PathBuf) -> AddOutcome {
        if self.folders.iter().any(|f| folder.starts_with(f)) {
            return AddOutcome::AlreadyIncluded;
        }
        let before = self.folders.len();
        self.folders.retain(|f| !f.starts_with(&folder));
        let merged = before - self.folders.len();
        self.folders.push(folder);
        self.folders.sort();
        if merged > 0 {
            AddOutcome::Merged(merged)
        } else {
            AddOutcome::Added
        }
    }

    pub fn remove(&mut self, folder: &Path) {
        self.folders.retain(|f| f != folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_nested() {
        let mut lib = Library::default();
        assert_eq!(lib.add("/home/u/Documents/Work".into()), AddOutcome::Added);
        assert_eq!(lib.add("/home/u/Documents/Taxes".into()), AddOutcome::Added);
        assert_eq!(lib.add("/home/u/Documents/Work/2024".into()), AddOutcome::AlreadyIncluded);
        assert_eq!(lib.add("/home/u/Documents".into()), AddOutcome::Merged(2));
        assert_eq!(lib.folders, vec![PathBuf::from("/home/u/Documents")]);
        // A sibling that merely shares a name prefix is not nested.
        assert_eq!(lib.add("/home/u/Documents2".into()), AddOutcome::Added);
    }
}
