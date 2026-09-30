//! User preferences, stored as JSON in the config directory.

use std::path::{Path, PathBuf};

use filefind_core::{IndexOptions, Sort};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub word_forms: bool,
    pub max_results: usize,
    pub file_names: bool,
    pub hidden_files: bool,
    /// Keep indexing when the window is closed, and start at login.
    pub background: bool,
    /// `None` means the default location in the app's data directory.
    pub index_dir: Option<PathBuf>,
    pub sort: SortOrder,
    /// Interface language code; `None` follows the system.
    pub language: Option<String>,
    pub window: WindowState,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            word_forms: true,
            max_results: 100,
            file_names: true,
            hidden_files: false,
            background: false,
            index_dir: None,
            sort: SortOrder::Relevance,
            language: None,
            window: WindowState::default(),
        }
    }
}

impl Settings {
    pub fn load(file: &Path) -> Settings {
        filefind_core::library::load_json(file)
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        filefind_core::library::save_json(file, self)
    }

    pub fn index_options(&self) -> IndexOptions {
        IndexOptions { file_names: self.file_names, hidden_files: self.hidden_files }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SortOrder {
    Relevance,
    Newest,
    Oldest,
    Name,
    Largest,
}

impl SortOrder {
    pub const ALL: [SortOrder; 5] = [SortOrder::Relevance, SortOrder::Newest, SortOrder::Oldest, SortOrder::Name, SortOrder::Largest];

    pub fn id(self) -> &'static str {
        match self {
            SortOrder::Relevance => "relevance",
            SortOrder::Newest => "newest",
            SortOrder::Oldest => "oldest",
            SortOrder::Name => "name",
            SortOrder::Largest => "largest",
        }
    }

    pub fn from_id(id: &str) -> Option<SortOrder> {
        SortOrder::ALL.into_iter().find(|s| s.id() == id)
    }

    pub fn to_core(self) -> Sort {
        match self {
            SortOrder::Relevance => Sort::Relevance,
            SortOrder::Newest => Sort::Newest,
            SortOrder::Oldest => Sort::Oldest,
            SortOrder::Name => Sort::Name,
            SortOrder::Largest => Sort::Largest,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub width: i32,
    pub height: i32,
    pub maximized: bool,
    pub sidebar: bool,
    pub sidebar_width: f64,
    pub preview_width: f64,
}

impl Default for WindowState {
    fn default() -> Self {
        WindowState { width: 1060, height: 700, maximized: false, sidebar: true, sidebar_width: 260.0, preview_width: 420.0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_use_defaults() {
        let s: Settings = serde_json::from_str(r#"{"background":true,"sort":"newest"}"#).unwrap();
        assert!(s.background);
        assert_eq!(s.sort, SortOrder::Newest);
        assert!(s.word_forms);
        assert_eq!(s.max_results, 100);
    }
}
