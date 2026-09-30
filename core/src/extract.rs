//! Text extraction for the document formats Filefind understands.
//!
//! Plain-text and zip/XML based formats are parsed in-process. Binary formats
//! with complex parsers (PDF, legacy Word) can be run in a helper subprocess so
//! that a malformed file can never crash, hang or exhaust the memory of the app.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use quick_xml::events::Event;
use quick_xml::Reader;

/// Maximum amount of extracted text kept per file.
pub const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;
/// Plain-text files larger than this are skipped (logs, dumps, ...).
const MAX_PLAIN_FILE: u64 = 32 * 1024 * 1024;
/// Documents larger than this are skipped.
const MAX_DOC_FILE: u64 = 256 * 1024 * 1024;
/// Upper bound for a single decompressed zip member.
const MAX_ZIP_MEMBER: u64 = 64 * 1024 * 1024;
/// How long the helper process may spend on one file.
const HELPER_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Text,
    Html,
    Rtf,
    Pdf,
    Odf,
    Docx,
    Xlsx,
    Pptx,
    Epub,
    Doc,
}

impl Kind {
    /// Whether this format should be parsed in an isolated helper process.
    fn isolated(self) -> bool {
        matches!(self, Kind::Pdf | Kind::Doc)
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

pub fn kind_for(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    let kind = match ext.as_str() {
        "pdf" => Kind::Pdf,
        "odt" | "ott" | "ods" | "ots" | "odp" | "otp" | "odg" | "fodt" => Kind::Odf,
        "docx" | "docm" | "dotx" => Kind::Docx,
        "xlsx" | "xlsm" => Kind::Xlsx,
        "pptx" | "pptm" => Kind::Pptx,
        "epub" => Kind::Epub,
        "doc" | "dot" => Kind::Doc,
        "rtf" => Kind::Rtf,
        "html" | "htm" | "xhtml" => Kind::Html,
        e if TEXT_EXTS.contains(&e) => Kind::Text,
        _ => return None,
    };
    Some(kind)
}

/// Whether a file of this kind and size should be indexed at all.
pub fn is_indexable(kind: Kind, size: u64) -> bool {
    size <= kind.max_file_size()
}

#[derive(Debug)]
pub struct ExtractError(pub String);

impl std::fmt::Display for ExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ExtractError {}

impl From<std::io::Error> for ExtractError {
    fn from(e: std::io::Error) -> Self {
        ExtractError(e.to_string())
    }
}

type Result<T> = std::result::Result<T, ExtractError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(ExtractError(msg.into()))
}

/// Extracts text, optionally isolating risky parsers in a helper process.
#[derive(Clone, Debug, Default)]
pub struct Extractor {
    /// Executable that implements [`helper_main`] when invoked with `--extract <path>`.
    pub helper: Option<PathBuf>,
}

impl Extractor {
    pub fn extract(&self, path: &Path, kind: Kind) -> Result<String> {
        match (&self.helper, kind.isolated()) {
            (Some(helper), true) => extract_in_helper(helper, path),
            _ => extract_guarded(path, kind),
        }
    }
}

/// Runs the in-process extractor, converting panics into errors.
pub fn extract_guarded(path: &Path, kind: Kind) -> Result<String> {
    std::panic::catch_unwind(|| extract_in_process(path, kind))
        .unwrap_or_else(|_| err("extractor panicked"))
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
        Kind::Xlsx => zip_xml_text(path, |name| {
            name == "xl/sharedStrings.xml"
                || (name.starts_with("xl/worksheets/sheet") && name.ends_with(".xml"))
        })?,
        Kind::Pptx => zip_xml_text(path, |name| {
            (name.starts_with("ppt/slides/slide") || name.starts_with("ppt/notesSlides/"))
                && name.ends_with(".xml")
        })?,
        Kind::Epub => zip_html_text(path)?,
        Kind::Doc => crate::msdoc::extract(path).map_err(|e| ExtractError(e.to_string()))?,
    };
    truncate_at_char_boundary(&mut text, MAX_TEXT_BYTES);
    Ok(text)
}

fn pdf_to_text(path: &Path) -> Result<String> {
    let bytes = read_capped(path, MAX_DOC_FILE)?;
    let text = pdf_extract::extract_text_from_mem(&bytes).map_err(|e| ExtractError(e.to_string()))?;
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
        zip::ZipArchive::new(File::open(path)?).map_err(|e| ExtractError(e.to_string()))?;
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
        zip::ZipArchive::new(File::open(path)?).map_err(|e| ExtractError(e.to_string()))?;
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
    let member = archive.by_name(name).map_err(|e| ExtractError(e.to_string()))?;
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

/// A forgiving HTML-to-text converter: drops tags, scripts and styles, decodes common entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        push_html_text(&mut out, &rest[..lt]);
        rest = &rest[lt..];
        let lower: String = rest.chars().take(10).collect::<String>().to_ascii_lowercase();
        let skip_to = if lower.starts_with("<!--") {
            Some("-->")
        } else if lower.starts_with("<script") {
            Some("</script")
        } else if lower.starts_with("<style") {
            Some("</style")
        } else {
            None
        };
        if let Some(end) = skip_to {
            let pos = find_ci(rest, end).map(|p| p + end.len()).unwrap_or(rest.len());
            rest = &rest[pos..];
            if let Some(gt) = rest.find('>').filter(|_| end != "-->") {
                rest = &rest[gt + 1..];
            }
            continue;
        }
        match rest.find('>') {
            Some(gt) => {
                let tag = lower.trim_start_matches(['<', '/']);
                if ["p", "br", "div", "li", "tr", "h1", "h2", "h3", "h4", "h5", "h6"]
                    .iter()
                    .any(|t| tag.starts_with(t) && tag[t.len()..].starts_with([' ', '>', '/']))
                {
                    out.push('\n');
                } else {
                    out.push(' ');
                }
                rest = &rest[gt + 1..];
            }
            None => break,
        }
    }
    push_html_text(&mut out, rest);
    out
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

fn extract_in_helper(helper: &Path, path: &Path) -> Result<String> {
    use std::os::unix::process::CommandExt;
    use wait_timeout::ChildExt;

    let mut cmd = Command::new(helper);
    cmd.arg("--extract").arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    unsafe {
        cmd.pre_exec(|| {
            // Keep a runaway parser from taking the machine down with it.
            let limit = libc::rlimit { rlim_cur: 3 << 30, rlim_max: 3 << 30 };
            libc::setrlimit(libc::RLIMIT_AS, &limit);
            libc::setpriority(libc::PRIO_PROCESS, 0, 15);
            Ok(())
        });
    }
    let mut child = cmd.spawn()?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = (&mut stdout).take(MAX_TEXT_BYTES as u64 + 16).read_to_end(&mut buf);
        buf
    });
    let status = match child.wait_timeout(HELPER_TIMEOUT)? {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return err("timed out");
        }
    };
    let bytes = reader.join().unwrap_or_default();
    if !status.success() {
        return err(format!("helper failed ({status})"));
    }
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    truncate_at_char_boundary(&mut text, MAX_TEXT_BYTES);
    Ok(text)
}

/// Entry point for the helper process (`<exe> --extract <path>`).
/// Writes the extracted text to stdout and returns the process exit code.
pub fn helper_main(path: &Path) -> i32 {
    // Anything a library prints must not end up in our output channel:
    // keep the real stdout aside and point fd 1 at stderr.
    let out_fd = unsafe {
        let fd = libc::dup(1);
        libc::dup2(2, 1);
        fd
    };
    if out_fd < 0 {
        return 3;
    }
    let Some(kind) = kind_for(path) else { return 2 };
    match extract_in_process(path, kind) {
        Ok(text) => {
            use std::os::fd::FromRawFd;
            let mut out = unsafe { File::from_raw_fd(out_fd) };
            if out.write_all(text.as_bytes()).is_err() {
                return 4;
            }
            0
        }
        Err(_) => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html() {
        let t = html_to_text("<html><style>x{}</style><p>Hello&nbsp;<b>world</b> &amp; you</p><script>bad()</script>end");
        assert!(t.contains("Hello  world  & you"), "{t:?}");
        assert!(!t.contains("bad"));
        assert!(t.contains("end"));
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
