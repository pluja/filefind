//! Text extraction for the document formats Filefind understands.
//!
//! Plain-text and zip/XML based formats are parsed in-process. Binary formats
//! with complex parsers (PDF, legacy Word) can be run in a helper subprocess so
//! that a malformed file can never crash, hang or exhaust the memory of the app.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::helper::HelperPool;

/// Maximum amount of extracted text kept per file.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Plain-text files larger than this are skipped (logs, dumps, ...).
const MAX_PLAIN_FILE: u64 = 32 * 1024 * 1024;
/// Documents larger than this are skipped.
const MAX_DOC_FILE: u64 = 256 * 1024 * 1024;
/// Upper bound for a single decompressed zip member.
const MAX_ZIP_MEMBER: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    Rtf,
    Pdf,
    Odf,
    Docx,
    Spreadsheet,
    Pptx,
    Epub,
    Doc,
}

impl Kind {
    /// Whether this format should be parsed in an isolated helper process.
    pub(crate) fn isolated(self) -> bool {
        matches!(self, Kind::Pdf | Kind::Doc | Kind::Spreadsheet)
    }

    fn max_file_size(self) -> u64 {
        match self {
            Kind::Text | Kind::Html | Kind::Rtf => MAX_PLAIN_FILE,
            _ => MAX_DOC_FILE,
        }
    }
}

const TEXT_EXTS: &[&str] = &[
    "txt", "text", "md", "markdown", "rst", "org", "adoc", "asciidoc", "tex", "bib", "csv", "tsv",
    "log", "json", "xml", "yaml", "yml", "toml", "ini", "cfg", "conf", "srt", "vtt", "sub", "nfo",
    "rs", "py", "js", "ts", "jsx", "tsx", "c", "h", "cc", "cpp", "hpp", "java", "kt", "go", "rb",
    "php", "sh", "bash", "zsh", "fish", "lua", "pl", "swift", "cs", "sql", "css", "scss", "vue",
    "svelte", "dart", "r", "m", "scala", "zig", "nim", "ex", "exs", "erl", "hs", "ml", "el",
    "clj", "vim", "diff", "patch", "desktop", "properties", "gradle", "cmake", "mk",
];

pub fn is_text_extension(ext: &str) -> bool {
    TEXT_EXTS.contains(&ext)
}

pub fn kind_for(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let kind = match ext.as_str() {
        "pdf" => Kind::Pdf,
        "odt" | "ott" | "odp" | "otp" | "odg" | "fodt" => Kind::Odf,
        "docx" | "docm" | "dotx" => Kind::Docx,
        "xlsx" | "xlsm" | "xlsb" | "xls" | "ods" | "ots" => Kind::Spreadsheet,
        "pptx" | "pptm" => Kind::Pptx,
        "epub" => Kind::Epub,
        "doc" | "dot" => Kind::Doc,
        "rtf" => Kind::Rtf,
        "html" | "htm" | "xhtml" => Kind::Html,
        e if is_text_extension(e) => Kind::Text,
        _ => return None,
    };
    Some(kind)
}

/// Whether a file of this kind and size should be indexed at all.
pub fn is_indexable(kind: Kind, size: u64) -> bool {
    size <= kind.max_file_size()
}

/// Why a file's text could not be read, in terms a user can act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    Encrypted,
    TimedOut,
    Crashed,
    Unreadable,
}

impl Failure {
    pub fn id(self) -> &'static str {
        match self {
            Failure::Encrypted => "encrypted",
            Failure::TimedOut => "timed-out",
            Failure::Crashed => "crashed",
            Failure::Unreadable => "unreadable",
        }
    }

    pub fn from_id(id: &str) -> Failure {
        [Failure::Encrypted, Failure::TimedOut, Failure::Crashed]
            .into_iter()
            .find(|f| f.id() == id)
            .unwrap_or(Failure::Unreadable)
    }
}

#[derive(Debug)]
pub struct ExtractError {
    pub failure: Failure,
    pub detail: String,
    /// The problem was not with the file (e.g. a helper couldn't start); try it again later.
    pub transient: bool,
}

impl ExtractError {
    pub fn new(failure: Failure, detail: impl Into<String>) -> ExtractError {
        ExtractError { failure, detail: detail.into(), transient: false }
    }

    pub fn transient(detail: impl Into<String>) -> ExtractError {
        ExtractError { failure: Failure::Crashed, detail: detail.into(), transient: true }
    }

    /// Classifies a parser's error message.
    fn from_message(detail: impl ToString) -> ExtractError {
        let detail = detail.to_string();
        let lower = detail.to_lowercase();
        let failure = if lower.contains("encrypt") || lower.contains("password") {
            Failure::Encrypted
        } else {
            Failure::Unreadable
        };
        ExtractError { failure, detail, transient: false }
    }
}

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.failure.id(), self.detail)
    }
}

impl std::error::Error for ExtractError {}

impl From<std::io::Error> for ExtractError {
    fn from(e: std::io::Error) -> Self {
        ExtractError::from_message(e)
    }
}

pub type Result<T> = std::result::Result<T, ExtractError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ExtractError::new(Failure::Unreadable, msg))
}

/// Extracts text, optionally isolating risky parsers in helper processes.
#[derive(Default)]
pub struct Extractor {
    helpers: Option<HelperPool>,
}

impl Extractor {
    pub fn in_process() -> Extractor {
        Extractor::default()
    }

    /// Parses risky formats in helper processes at idle priority, for indexing.
    /// `exe` must run [`crate::helper::serve`] when started with [`crate::helper::SERVER_ARG`].
    pub fn with_helper(exe: PathBuf) -> Extractor {
        Extractor { helpers: Some(HelperPool::new(exe, true)) }
    }

    /// Like [`Extractor::with_helper`], at normal priority, for something the user waits on.
    /// Call [`Extractor::release_idle`] after use from short-lived threads: helpers are
    /// killed when the thread that started them exits.
    pub fn with_interactive_helper(exe: PathBuf) -> Extractor {
        Extractor { helpers: Some(HelperPool::new(exe, false)) }
    }

    pub fn extract(&self, path: &Path, kind: Kind) -> Result<String> {
        match &self.helpers {
            Some(pool) if kind.isolated() => pool.extract(path),
            _ => extract_guarded(path, kind),
        }
    }

    /// Stops idle helper processes, returning their memory to the system.
    pub fn release_idle(&self) {
        if let Some(pool) = &self.helpers {
            pool.release_idle();
        }
    }
}

/// Runs the in-process extractor, converting panics into errors.
pub fn extract_guarded(path: &Path, kind: Kind) -> Result<String> {
    std::panic::catch_unwind(|| extract_in_process(path, kind))
        .unwrap_or_else(|_| Err(ExtractError::new(Failure::Crashed, "parser panicked")))
}

pub fn extract_in_process(path: &Path, kind: Kind) -> Result<String> {
    let mut text = match kind {
        Kind::Text => decode_text(&read_capped(path, MAX_PLAIN_FILE)?)?,
        Kind::Html => html_to_text(&decode_text(&read_capped(path, MAX_PLAIN_FILE)?)?),
        Kind::Rtf => rtf_to_text(&String::from_utf8_lossy(&read_capped(path, MAX_PLAIN_FILE)?)),
        Kind::Pdf => pdf_to_text(path)?,
        Kind::Odf => {
            if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("fodt")) {
                xml_to_text(&read_capped(path, MAX_PLAIN_FILE)?)
            } else {
                zip_xml_text(path, |name| name == "content.xml")?
            }
        }
        Kind::Docx => zip_xml_text(path, |name| {
            name == "word/document.xml"
                || name.starts_with("word/header")
                || name.starts_with("word/footer")
                || name == "word/footnotes.xml"
        })?,
        Kind::Spreadsheet => spreadsheet_to_text(path)?,
        Kind::Pptx => zip_xml_text(path, |name| {
            (name.starts_with("ppt/slides/slide") || name.starts_with("ppt/notesSlides/"))
                && name.ends_with(".xml")
        })?,
        Kind::Epub => zip_html_text(path)?,
        Kind::Doc => crate::msdoc::extract(path).map_err(ExtractError::from_message)?,
    };
    truncate_at_char_boundary(&mut text, MAX_TEXT_BYTES);
    Ok(text)
}

/// One line per row with tab-separated cells; with several sheets, each starts with its name.
fn spreadsheet_to_text(path: &Path) -> Result<String> {
    use calamine::Reader;
    let mut book = calamine::open_workbook_auto(path).map_err(ExtractError::from_message)?;
    let names = book.sheet_names();
    let mut out = String::new();
    for name in &names {
        let Ok(range) = book.worksheet_range(name) else { continue };
        if names.len() > 1 {
            out.push_str(name);
            out.push('\n');
        }
        for row in range.rows() {
            let cells: Vec<String> = row.iter().map(|c| c.to_string().replace(['\t', '\n', '\r'], " ")).collect();
            let used = cells.iter().rposition(|c| !c.is_empty()).map_or(0, |i| i + 1);
            out.push_str(&cells[..used].join("\t"));
            out.push('\n');
            if out.len() > MAX_TEXT_BYTES {
                return Ok(out);
            }
        }
        out.push('\n');
    }
    Ok(out)
}

fn pdf_to_text(path: &Path) -> Result<String> {
    let bytes = read_capped(path, MAX_DOC_FILE)?;
    let text = pdf_extract::extract_text_from_mem(&bytes).map_err(ExtractError::from_message)?;
    // pdf-extract emits form feeds and ragged spacing; compact it a little.
    Ok(text.replace('\u{c}', "\n"))
}

pub fn truncate_at_char_boundary(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
    }
}

fn read_capped(path: &Path, max: u64) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    File::open(path)?.take(max).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Decodes a text file: UTF-8, UTF-16 (with BOM), or Windows-1252 as a fallback.
/// Files that look binary are rejected.
pub fn decode_text(bytes: &[u8]) -> Result<String> {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return Ok(String::from_utf8_lossy(rest).into_owned());
    }
    if bytes.len() >= 2 && (bytes[..2] == [0xFF, 0xFE] || bytes[..2] == [0xFE, 0xFF]) {
        let le = bytes[0] == 0xFF;
        let units = bytes[2..].as_chunks::<2>().0.iter().map(|c| {
            if le {
                u16::from_le_bytes([c[0], c[1]])
            } else {
                u16::from_be_bytes([c[0], c[1]])
            }
        });
        return Ok(char::decode_utf16(units)
            .map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect());
    }
    let head = &bytes[..bytes.len().min(8192)];
    if head.contains(&0) {
        return err("binary file");
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_owned()),
        Err(e) if e.error_len().is_none() && bytes.len() as u64 >= MAX_PLAIN_FILE => {
            // Truncated multi-byte sequence at the end of a capped read.
            Ok(String::from_utf8_lossy(bytes).into_owned())
        }
        Err(_) => Ok(bytes.iter().map(|&b| cp1252(b)).collect()),
    }
}

/// Maps a Windows-1252 byte to a char.
pub fn cp1252(b: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160,
        0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    match b {
        0x80..=0x9F => char::from_u32(HIGH[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}'),
        _ => b as char,
    }
}

/// Reads the zip members selected by `wanted` (in archive order) and extracts their XML text.
fn zip_xml_text(path: &Path, wanted: impl Fn(&str) -> bool) -> Result<String> {
    let mut archive =
        zip::ZipArchive::new(File::open(path)?).map_err(ExtractError::from_message)?;
    let mut names: Vec<String> = archive.file_names().filter(|n| wanted(n)).map(String::from).collect();
    // Keep slides/sheets in natural order (slide2 before slide10).
    names.sort_by_key(|n| natural_key(n));
    let mut out = String::new();
    for name in names {
        let bytes = read_zip_member(&mut archive, &name)?;
        out.push_str(&xml_to_text(&bytes));
        out.push('\n');
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    Ok(out)
}

fn zip_html_text(path: &Path) -> Result<String> {
    let mut archive =
        zip::ZipArchive::new(File::open(path)?).map_err(ExtractError::from_message)?;
    let mut names: Vec<String> = archive
        .file_names()
        .filter(|n| {
            let n = n.to_ascii_lowercase();
            n.ends_with(".xhtml") || n.ends_with(".html") || n.ends_with(".htm")
        })
        .map(String::from)
        .collect();
    names.sort_by_key(|n| natural_key(n));
    let mut out = String::new();
    for name in names {
        let bytes = read_zip_member(&mut archive, &name)?;
        out.push_str(&html_to_text(&String::from_utf8_lossy(&bytes)));
        out.push('\n');
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    Ok(out)
}

fn read_zip_member(archive: &mut zip::ZipArchive<File>, name: &str) -> Result<Vec<u8>> {
    let member = archive.by_name(name).map_err(ExtractError::from_message)?;
    let mut buf = Vec::new();
    member.take(MAX_ZIP_MEMBER).read_to_end(&mut buf)?;
    Ok(buf)
}

fn natural_key(name: &str) -> (String, u64) {
    let digits: String = name.chars().rev().skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit()).collect::<Vec<_>>().into_iter().rev().collect();
    let prefix = name.trim_end_matches(|c: char| !c.is_ascii_digit())
        .trim_end_matches(|c: char| c.is_ascii_digit()).to_owned();
    (prefix, digits.parse().unwrap_or(0))
}

/// Extracts human-readable text from office XML (ODF / OOXML), inserting
/// line breaks at paragraph boundaries and tabs between table cells.
pub fn xml_to_text(bytes: &[u8]) -> String {
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut out = String::new();
    let mut buf = Vec::new();
    // Skip text inside these elements (metadata, embedded binary, field instructions).
    let mut skip_depth = 0usize;
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e.local_name();
                if skip_depth > 0 || matches!(name.as_ref(), b"binData" | b"instrText" | b"del" | b"delText") {
                    skip_depth += 1;
                }
            }
            Ok(Event::End(e)) => {
                if skip_depth > 0 {
                    skip_depth -= 1;
                    continue;
                }
                match e.local_name().as_ref() {
                    b"p" | b"h" | b"si" | b"tr" | b"list-item" | b"title" => out.push('\n'),
                    b"tc" | b"table-cell" | b"c" => out.push('\t'),
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => {
                if skip_depth == 0 {
                    match e.local_name().as_ref() {
                        b"tab" => out.push('\t'),
                        b"br" | b"line-break" | b"cr" => out.push('\n'),
                        b"s" => out.push(' '),
                        b"p" | b"h" => out.push('\n'),
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if skip_depth == 0 {
                    if let Ok(s) = t.decode() {
                        out.push_str(&s);
                    }
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if skip_depth == 0 {
                    push_entity(&mut out, &String::from_utf8_lossy(&r));
                }
            }
            Ok(Event::CData(t)) => {
                if skip_depth == 0 {
                    out.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break, // Keep whatever was extracted before the error.
            _ => {}
        }
        buf.clear();
        if out.len() > MAX_TEXT_BYTES {
            break;
        }
    }
    out
}

fn push_entity(out: &mut String, name: &str) {
    let c = match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        n if n.starts_with("#x") || n.starts_with("#X") => {
            u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32)
        }
        n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
        _ => None,
    };
    if let Some(c) = c {
        out.push(c);
    }
}

/// Converts HTML to readable, Markdown-like text: headings become `#` lines, list items
/// `-` lines, paragraphs are separated by blank lines, and source whitespace is collapsed
/// as a browser would. Scripts, styles and the document head are dropped.
pub fn html_to_text(html: &str) -> String {
    let mut out = HtmlText::default();
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.text(&rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |end| &rest[end + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else { break };
        let closing = rest[1..].starts_with('/');
        let name: String = rest[1 + closing as usize..]
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect::<String>()
            .to_ascii_lowercase();
        rest = &rest[gt + 1..];
        if !closing && matches!(name.as_str(), "script" | "style" | "head" | "noscript" | "svg" | "template") {
            let end = format!("</{name}");
            rest = find_ci(rest, &end).and_then(|p| rest[p..].find('>').map(|g| &rest[p + g + 1..])).unwrap_or("");
            continue;
        }
        out.tag(&name, closing);
    }
    out.text(rest);
    out.finish()
}

#[derive(Default)]
struct HtmlText {
    out: String,
    list_depth: usize,
    in_pre: bool,
}

impl HtmlText {
    fn text(&mut self, raw: &str) {
        let mut decoded = String::with_capacity(raw.len());
        push_html_text(&mut decoded, raw);
        if self.in_pre {
            self.out.push_str(&decoded);
            return;
        }
        for (i, word) in decoded.split_whitespace().enumerate() {
            let at_line_start = self.out.is_empty() || self.out.ends_with('\n') || self.out.ends_with("- ") || self.out.ends_with("# ");
            let spaced = i > 0 || decoded.starts_with(char::is_whitespace);
            if spaced && !at_line_start && !self.out.ends_with(' ') {
                self.out.push(' ');
            }
            self.out.push_str(word);
        }
        if decoded.ends_with(char::is_whitespace) && !self.out.ends_with(['\n', ' ']) && !self.out.is_empty() {
            self.out.push(' ');
        }
    }

    /// Ends the current line; with `blank`, also leaves an empty line.
    fn block(&mut self, blank: bool) {
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        if self.out.is_empty() {
            return;
        }
        let wanted = if blank { "\n\n" } else { "\n" };
        while !self.out.ends_with(wanted) {
            self.out.push('\n');
        }
    }

    fn tag(&mut self, name: &str, closing: bool) {
        match name {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.block(true);
                if !closing {
                    let level = name[1..].parse().unwrap_or(1);
                    self.out.push_str(&"#".repeat(level));
                    self.out.push(' ');
                }
            }
            "ul" | "ol" => {
                if closing {
                    self.list_depth = self.list_depth.saturating_sub(1);
                } else {
                    self.list_depth += 1;
                }
                self.block(self.list_depth == 0);
            }
            "li" if !closing => {
                self.block(false);
                self.out.push_str(&"  ".repeat(self.list_depth.saturating_sub(1)));
                self.out.push_str("- ");
            }
            "li" | "tr" | "dt" | "dd" => self.block(false),
            "td" | "th" if !closing && !self.out.ends_with('\n') => self.out.push_str(" · "),
            "br" => self.out.push('\n'),
            "hr" => {
                self.block(true);
                self.out.push_str("---");
                self.block(true);
            }
            "pre" => {
                self.block(true);
                self.out.push_str("```");
                self.block(false);
                if closing {
                    self.block(true);
                }
                self.in_pre = !closing;
            }
            "p" | "div" | "section" | "article" | "header" | "footer" | "nav" | "main" | "aside" | "table" | "blockquote"
            | "figure" | "form" | "dl" | "title" => self.block(self.list_depth == 0),
            _ => {}
        }
    }

    fn finish(self) -> String {
        let mut out = String::with_capacity(self.out.len());
        let mut blank_lines = 0;
        for line in self.out.lines() {
            let line = line.trim_end();
            if line.trim().is_empty() {
                blank_lines += 1;
                if blank_lines > 1 {
                    continue;
                }
            } else {
                blank_lines = 0;
            }
            out.push_str(line);
            out.push('\n');
        }
        out.trim().to_owned()
    }
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.as_bytes().windows(needle.len()).position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

fn push_html_text(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        match rest[1..].find(';').filter(|&p| p <= 10) {
            Some(p) => {
                let before = out.len();
                push_entity(out, &rest[1..p + 1]);
                if out.len() == before {
                    out.push_str(&rest[..p + 2]);
                }
                rest = &rest[p + 2..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Minimal RTF reader: keeps plain text, decodes \'hh and \uN escapes, skips destinations.
pub fn rtf_to_text(rtf: &str) -> String {
    let bytes = rtf.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    // Stack of "skip this group" flags.
    let mut skip_stack: Vec<bool> = vec![false];
    let mut uc_skip = 0usize;
    let skipped_destinations: &[&str] = &[
        "fonttbl", "colortbl", "stylesheet", "info", "pict", "object", "header", "footer",
        "listtable", "listoverridetable", "rsidtbl", "generator", "xmlnstbl", "themedata",
        "colorschememapping", "latentstyles", "datastore", "fldinst",
    ];
    while i < bytes.len() {
        let skipping = *skip_stack.last().unwrap_or(&false);
        match bytes[i] {
            b'{' => {
                skip_stack.push(skipping);
                i += 1;
            }
            b'}' => {
                if skip_stack.len() > 1 {
                    skip_stack.pop();
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    break;
                }
                match bytes[i] {
                    b'\\' | b'{' | b'}' => {
                        if !skipping {
                            out.push(bytes[i] as char);
                        }
                        i += 1;
                    }
                    b'*' => {
                        if let Some(top) = skip_stack.last_mut() {
                            *top = true;
                        }
                        i += 1;
                    }
                    b'\'' => {
                        if let Some(b) = rtf.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                            if !skipping && uc_skip == 0 {
                                out.push(cp1252(b));
                            }
                            uc_skip = uc_skip.saturating_sub(1);
                        }
                        i += 3;
                    }
                    b'\n' | b'\r' => {
                        if !skipping {
                            out.push('\n');
                        }
                        i += 1;
                    }
                    c if c.is_ascii_alphabetic() => {
                        let start = i;
                        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
                            i += 1;
                        }
                        let word = &rtf[start..i];
                        let num_start = i;
                        if i < bytes.len() && bytes[i] == b'-' {
                            i += 1;
                        }
                        while i < bytes.len() && bytes[i].is_ascii_digit() {
                            i += 1;
                        }
                        let num: Option<i32> = rtf[num_start..i].parse().ok();
                        if i < bytes.len() && bytes[i] == b' ' {
                            i += 1;
                        }
                        if skipped_destinations.contains(&word) {
                            if let Some(top) = skip_stack.last_mut() {
                                *top = true;
                            }
                            continue;
                        }
                        if skipping {
                            continue;
                        }
                        match word {
                            "par" | "line" | "row" | "sect" | "page" => out.push('\n'),
                            "tab" | "cell" => out.push('\t'),
                            "u" => {
                                if let Some(n) = num {
                                    let code = if n < 0 { (n + 65536) as u32 } else { n as u32 };
                                    out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
                                    uc_skip = 1;
                                }
                            }
                            _ => {}
                        }
                    }
                    _ => i += 1,
                }
            }
            b'\r' | b'\n' => i += 1,
            _ => {
                let start = i;
                while i < bytes.len() && !matches!(bytes[i], b'\\' | b'{' | b'}' | b'\r' | b'\n') {
                    i += 1;
                }
                if !skipping {
                    let mut chunk = &rtf[start..i];
                    if uc_skip > 0 {
                        let mut chars = chunk.chars();
                        chars.next();
                        chunk = chars.as_str();
                        uc_skip = 0;
                    }
                    out.push_str(chunk);
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html() {
        let t = html_to_text(
            "<html><head><title>T</title><style>x{}</style></head><body>\n  <h1>Title</h1>\n  <p>Hello&nbsp;<b>world</b>\n  &amp; you</p>\n\n\n<ul>\n <li>one</li>\n <li>two <a href=x>link</a></li></ul><script>bad()</script>end</body>",
        );
        assert_eq!(t, "# Title\n\nHello world & you\n\n- one\n- two link\n\nend");
    }

    #[test]
    fn rtf() {
        let t = rtf_to_text(r"{\rtf1\ansi{\fonttbl{\f0 Arial;}}\f0 Hello \b world\b0\par Caf\'e9 \u8364?}");
        assert!(t.contains("Hello world"), "{t:?}");
        assert!(t.contains("Café €"), "{t:?}");
        assert!(!t.contains("Arial"));
    }

    #[test]
    fn xml() {
        let t = xml_to_text(br#"<w:document xmlns:w="x"><w:p><w:r><w:t>Hello</w:t></w:r><w:tab/><w:r><w:t>A &amp; B</w:t></w:r></w:p><w:p><w:t>Next</w:t></w:p></w:document>"#);
        assert_eq!(t, "Hello\tA & B\nNext\n");
    }

    #[test]
    fn decode() {
        assert_eq!(decode_text(b"caf\xe9").unwrap(), "café");
        assert!(decode_text(b"\x00\x01binary").is_err());
        assert_eq!(decode_text(b"\xff\xfeh\x00i\x00").unwrap(), "hi");
    }

    #[test]
    fn kinds() {
        assert_eq!(kind_for(Path::new("/a/B.PDF")), Some(Kind::Pdf));
        assert_eq!(kind_for(Path::new("/a/b.odt")), Some(Kind::Odf));
        assert_eq!(kind_for(Path::new("/a/b.png")), None);
        assert_eq!(kind_for(Path::new("/a/Makefile")), None);
    }
}
