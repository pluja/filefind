//! What gets indexed: options and exclusions.

use std::path::{Path, PathBuf};

/// Skipped unless the user removes them: dependency and system folders full of files
/// nobody searches for.
pub const DEFAULT_EXCLUDED_NAMES: [&str; 4] = ["node_modules", "__pycache__", "site-packages", "lost+found"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IndexOptions {
    /// Also index files that aren't documents (photos, music, archives…) by name.
    pub file_names: bool,
    pub hidden_files: bool,
    /// Folders skipped with everything inside them.
    pub excluded_folders: Vec<PathBuf>,
    /// Name patterns (`*` and `?` wildcards, any case) for files and folders to skip.
    pub excluded_names: Vec<String>,
}

impl Default for IndexOptions {
    fn default() -> Self {
        IndexOptions {
            file_names: true,
            hidden_files: false,
            excluded_folders: Vec::new(),
            excluded_names: DEFAULT_EXCLUDED_NAMES.map(String::from).to_vec(),
        }
    }
}

impl IndexOptions {
    /// Whether a file or folder with this name is skipped wherever it is.
    pub fn skips_name(&self, name: &str) -> bool {
        (!self.hidden_files && name.starts_with('.')) || self.excluded_names.iter().any(|p| glob_match(p, name))
    }

    /// Whether `path` is skipped, looking at its own name and at the excluded folders.
    pub fn skips(&self, path: &Path) -> bool {
        path.file_name().is_some_and(|n| self.skips_name(&n.to_string_lossy()))
            || self.excluded_folders.iter().any(|f| path.starts_with(f))
    }

    /// Whether anything between `root` and `path` (inclusive) is skipped.
    pub fn skips_below(&self, root: &Path, path: &Path) -> bool {
        let Ok(rel) = path.strip_prefix(root) else { return false };
        rel.components().any(|c| self.skips_name(&c.as_os_str().to_string_lossy()))
            || self.excluded_folders.iter().any(|f| path.starts_with(f))
    }
}

/// Matches `name` against `pattern`, where `*` is any run of characters and `?` any one
/// character. Case-insensitive.
pub fn glob_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.to_lowercase().chars().collect();
    let n: Vec<char> = name.to_lowercase().chars().collect();
    // Classic wildcard matching with backtracking to the last `*`.
    let (mut pi, mut ni) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ni < n.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some((pi, ni));
                pi += 1;
            }
            Some(&c) if c == '?' || c == n[ni] => {
                pi += 1;
                ni += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    pi = sp + 1;
                    ni = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("*.log", "server.LOG"));
        assert!(glob_match("draft-*", "draft-2024.docx"));
        assert!(glob_match("node_modules", "node_modules"));
        assert!(glob_match("a?c*", "abcdef"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("*.log", "log.txt"));
        assert!(!glob_match("draft-*", "my-draft-1"));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn exclusions() {
        let options = IndexOptions {
            excluded_folders: vec!["/home/u/Downloads/private".into()],
            excluded_names: vec!["*.tmp".into()],
            ..Default::default()
        };
        assert!(options.skips(Path::new("/home/u/Downloads/private")));
        assert!(options.skips(Path::new("/home/u/Downloads/private/a.pdf")));
        assert!(!options.skips(Path::new("/home/u/Downloads/private-not/a.pdf")));
        assert!(options.skips(Path::new("/home/u/x.TMP")));
        assert!(options.skips(Path::new("/home/u/.config")));
        assert!(options.skips_below(Path::new("/home/u"), Path::new("/home/u/.cache/a.txt")));
        assert!(!options.skips_below(Path::new("/home/.u"), Path::new("/home/.u/a.txt")), "only below the root");
    }
}
