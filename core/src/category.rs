//! Broad file categories, used for filtering results.

use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Category {
    Document,
    Pdf,
    Spreadsheet,
    Presentation,
    Text,
    Image,
    Audio,
    Video,
    Archive,
    Other,
}

impl Category {
    pub const ALL: [Category; 10] = [
        Category::Document,
        Category::Pdf,
        Category::Spreadsheet,
        Category::Presentation,
        Category::Text,
        Category::Image,
        Category::Audio,
        Category::Video,
        Category::Archive,
        Category::Other,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Category::Document => "document",
            Category::Pdf => "pdf",
            Category::Spreadsheet => "spreadsheet",
            Category::Presentation => "presentation",
            Category::Text => "text",
            Category::Image => "image",
            Category::Audio => "audio",
            Category::Video => "video",
            Category::Archive => "archive",
            Category::Other => "other",
        }
    }

    pub fn from_id(id: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.id() == id)
    }

    pub fn from_path(path: &Path) -> Category {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else { return Category::Other };
        Category::from_extension(&ext.to_ascii_lowercase())
    }

    /// `ext` must be lowercase.
    pub fn from_extension(ext: &str) -> Category {
        match ext {
            "doc" | "docx" | "docm" | "dot" | "dotx" | "odt" | "ott" | "fodt" | "rtf" | "epub" | "pages" | "wpd" => {
                Category::Document
            }
            "pdf" => Category::Pdf,
            "xls" | "xlsx" | "xlsm" | "ods" | "ots" | "csv" | "tsv" | "numbers" => Category::Spreadsheet,
            "ppt" | "pptx" | "pptm" | "odp" | "otp" | "key" => Category::Presentation,
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" | "tif" | "tiff" | "heic" | "heif" | "avif"
            | "raw" | "cr2" | "nef" | "arw" | "dng" | "psd" | "xcf" | "kra" | "ico" => Category::Image,
            "mp3" | "flac" | "ogg" | "oga" | "opus" | "wav" | "m4a" | "aac" | "wma" | "aiff" | "mid" | "midi" => {
                Category::Audio
            }
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "m4v" | "mpg" | "mpeg" | "3gp" | "ogv" => Category::Video,
            "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "iso" | "deb" | "rpm" | "dmg"
            | "appimage" | "flatpak" | "jar" => Category::Archive,
            e if crate::extract::is_text_extension(e) || matches!(e, "html" | "htm" | "xhtml") => Category::Text,
            _ => Category::Other,
        }
    }

    /// Resolves a `type:` word: a category name in English or Spanish, or a file extension.
    pub fn parse(word: &str) -> Option<TypeFilter> {
        let w = word.to_lowercase();
        let category = match w.as_str() {
            "doc" | "docs" | "document" | "documents" | "documento" | "documentos" => Some(Category::Document),
            "pdf" | "pdfs" => Some(Category::Pdf),
            "sheet" | "sheets" | "spreadsheet" | "spreadsheets" | "hoja" | "hojas" | "excel" => Some(Category::Spreadsheet),
            "slides" | "presentation" | "presentations" | "presentacion" | "presentación" | "presentaciones" => {
                Some(Category::Presentation)
            }
            "text" | "texto" | "code" | "codigo" | "código" => Some(Category::Text),
            "image" | "images" | "imagen" | "imagenes" | "imágenes" | "photo" | "photos" | "foto" | "fotos" => {
                Some(Category::Image)
            }
            "audio" | "music" | "musica" | "música" | "sound" => Some(Category::Audio),
            "video" | "videos" | "vídeo" | "vídeos" | "movie" | "movies" => Some(Category::Video),
            "archive" | "archives" | "archivo" | "comprimido" | "comprimidos" => Some(Category::Archive),
            _ => None,
        };
        match category {
            Some(c) => Some(TypeFilter::Category(c)),
            None if !w.is_empty() && w.chars().all(|c| c.is_ascii_alphanumeric()) => Some(TypeFilter::Extension(w)),
            None => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeFilter {
    Category(Category),
    Extension(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn categories() {
        assert_eq!(Category::from_path(Path::new("/a/Report.PDF")), Category::Pdf);
        assert_eq!(Category::from_path(Path::new("/a/notes.md")), Category::Text);
        assert_eq!(Category::from_path(Path::new("/a/photo.HEIC")), Category::Image);
        assert_eq!(Category::from_path(Path::new("/a/Makefile")), Category::Other);
        for c in Category::ALL {
            assert_eq!(Category::from_id(c.id()), Some(c));
        }
    }

    #[test]
    fn type_words() {
        assert_eq!(Category::parse("PDF"), Some(TypeFilter::Category(Category::Pdf)));
        assert_eq!(Category::parse("fotos"), Some(TypeFilter::Category(Category::Image)));
        assert_eq!(Category::parse("docx"), Some(TypeFilter::Extension("docx".into())));
        assert_eq!(Category::parse("a/b"), None);
    }
}
