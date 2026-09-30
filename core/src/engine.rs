//! The search index: schema, document construction and fuzzy querying.

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, STORED, STRING,
};
use tantivy::tokenizer::{
    AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer,
};
use tantivy::{Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term};

/// Bump whenever the schema or tokenization changes; the index is rebuilt on mismatch.
const SCHEMA_VERSION: &str = "1";
const TOKENIZER: &str = "ff";
/// Stored text used for result snippets.
const PREVIEW_BYTES: usize = 256 * 1024;
const SNIPPET_CHARS: usize = 220;

const NAME_BOOST: f32 = 2.5;
const DIR_BOOST: f32 = 0.8;
const CONTENT_BOOST: f32 = 1.0;
/// Upper bound on how many dictionary terms one query word may expand to, per field.
const MAX_EXPANSIONS: usize = 40;

fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(48))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

#[derive(Clone, Copy)]
pub struct Fields {
    pub path: Field,
    pub name: Field,
    pub dir: Field,
    pub content: Field,
    pub preview: Field,
    pub mtime: Field,
    pub size: Field,
    pub kind: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text = TextOptions::default().set_indexing_options(indexing);
    let fields = Fields {
        path: b.add_text_field("path", STRING | STORED | FAST),
        name: b.add_text_field("name", text.clone().set_stored()),
        dir: b.add_text_field("dir", text.clone()),
        content: b.add_text_field("content", text),
        preview: b.add_text_field("preview", STORED),
        mtime: b.add_u64_field("mtime", STORED | FAST),
        size: b.add_u64_field("size", STORED),
        kind: b.add_text_field("kind", STRING | STORED),
    };
    (b.build(), fields)
}

/// Metadata of a file on disk, as recorded in the index.
#[derive(Clone, Debug)]
pub struct FileMeta {
    pub mtime: u64,
    pub size: u64,
}

/// A piece of display text; `true` marks a highlighted match.
pub type Segment = (String, bool);

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: String,
    pub name: Vec<Segment>,
    pub snippet: Vec<Segment>,
    pub mtime: u64,
    pub size: u64,
    pub kind: String,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub total: usize,
    pub hits: Vec<Hit>,
    pub elapsed: Duration,
}

pub struct Engine {
    index: Index,
    reader: IndexReader,
    pub fields: Fields,
    lev: [OnceLock<LevenshteinAutomatonBuilder>; 2],
}

impl Engine {
    /// Opens the index in `dir`, recreating it if it is missing, outdated or corrupt.
    pub fn open(dir: &Path) -> tantivy::Result<Engine> {
        let (schema, fields) = build_schema();
        let version_file = dir.join("filefind-schema");
        let current = std::fs::read_to_string(&version_file).unwrap_or_default();

        let open_existing = || -> tantivy::Result<Index> {
            let index = Index::open_in_dir(dir)?;
            if index.schema() != schema {
                return Err(tantivy::TantivyError::SchemaError("schema changed".into()));
            }
            Ok(index)
        };
        let index = match (current.trim() == SCHEMA_VERSION).then(open_existing) {
            Some(Ok(index)) => index,
            other => {
                if let Some(Err(e)) = other {
                    log::warn!("index unusable ({e}), rebuilding");
                }
                let _ = std::fs::remove_dir_all(dir);
                std::fs::create_dir_all(dir)?;
                let index = Index::create(MmapDirectory::open(dir)?, schema, Default::default())?;
                std::fs::write(&version_file, SCHEMA_VERSION)?;
                index
            }
        };
        index.tokenizers().register(TOKENIZER, analyzer());
        let reader = index.reader_builder().reload_policy(ReloadPolicy::OnCommitWithDelay).try_into()?;
        Ok(Engine { index, reader, fields, lev: [OnceLock::new(), OnceLock::new()] })
    }

    pub fn writer(&self) -> tantivy::Result<IndexWriter> {
        self.index.writer_with_num_threads(2, 96_000_000)
    }

    pub fn reload(&self) {
        let _ = self.reader.reload();
    }

    pub fn num_docs(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    pub fn path_term(&self, path: &str) -> Term {
        Term::from_field_text(self.fields.path, path)
    }

    /// Deletes every document located below directory `dir`.
    pub fn delete_under(&self, writer: &IndexWriter, dir: &str) {
        let dir = dir.trim_end_matches('/');
        // '0' is the character right after '/', so this covers exactly "dir/...".
        let lower = Term::from_field_text(self.fields.path, &format!("{dir}/"));
        let upper = Term::from_field_text(self.fields.path, &format!("{dir}0"));
        let query = RangeQuery::new(Bound::Included(lower), Bound::Excluded(upper));
        if let Err(e) = writer.delete_query(Box::new(query)) {
            log::warn!("delete under {dir}: {e}");
        }
    }

    /// All indexed paths with their recorded modification time.
    pub fn existing(&self) -> HashMap<String, u64> {
        let searcher = self.reader.searcher();
        let mut out = HashMap::new();
        for segment in searcher.segment_readers() {
            let ff = segment.fast_fields();
            let (Ok(Some(paths)), Ok(mtimes)) = (ff.str("path"), ff.u64("mtime")) else { continue };
            let mut path = String::new();
            for doc in segment.doc_ids_alive() {
                let Some(ord) = paths.term_ords(doc).next() else { continue };
                path.clear();
                if paths.ord_to_str(ord, &mut path).unwrap_or(false) {
                    out.insert(path.clone(), mtimes.first(doc).unwrap_or(0));
                }
            }
        }
        out
    }

    /// Builds the index document for a file. `root` is the library folder containing it.
    pub fn make_doc(&self, path: &Path, root: &Path, meta: &FileMeta, text: &str) -> TantivyDocument {
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.path, path.to_string_lossy());
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        doc.add_text(f.name, &name);
        // Folder names are searchable too ("taxes 2023"), relative to the library folder.
        let mut dir_text = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(rel) = path.parent().and_then(|p| p.strip_prefix(root).ok()) {
            dir_text.push(' ');
            dir_text.push_str(&rel.to_string_lossy());
        }
        doc.add_text(f.dir, &dir_text);
        doc.add_text(f.content, text);
        let mut preview = text.to_owned();
        crate::extract::truncate_at_char_boundary(&mut preview, PREVIEW_BYTES);
        doc.add_text(f.preview, &preview);
        doc.add_u64(f.mtime, meta.mtime);
        doc.add_u64(f.size, meta.size);
        let kind = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        doc.add_text(f.kind, &kind);
        doc
    }

    fn lev(&self, distance: u8) -> &LevenshteinAutomatonBuilder {
        self.lev[distance as usize - 1].get_or_init(|| LevenshteinAutomatonBuilder::new(distance, true))
    }

    /// Finds dictionary terms in `field` that match `word` exactly, by prefix or within a small
    /// edit distance. Returns term texts with a score multiplier.
    fn expand(&self, searcher: &Searcher, field: Field, word: &str, prefix: bool) -> Vec<(String, f32)> {
        let mut found: HashMap<String, f32> = HashMap::new();
        found.insert(word.to_owned(), 1.0);

        let len = word.chars().count();
        let numeric = word.chars().all(|c| c.is_ascii_digit());
        let distance = match len {
            _ if numeric => 0,
            0..=3 => 0,
            4..=7 => 1,
            _ => 2,
        };
        let dfa: Option<DFA> = (distance > 0).then(|| self.lev(distance).build_dfa(word));

        for segment in searcher.segment_readers() {
            let Ok(inverted) = segment.inverted_index(field) else { continue };
            let terms = inverted.terms();
            if prefix && len >= 2 {
                if let Ok(mut stream) = terms.range().ge(word.as_bytes()).into_stream() {
                    let mut n = 0;
                    while stream.advance() && n < 64 {
                        let key = stream.key();
                        if !key.starts_with(word.as_bytes()) {
                            break;
                        }
                        if let Ok(s) = std::str::from_utf8(key) {
                            let e = found.entry(s.to_owned()).or_insert(0.0);
                            *e = e.max(0.6);
                        }
                        n += 1;
                    }
                }
            }
            if let Some(dfa) = &dfa {
                if let Ok(mut stream) = terms.search(DfaAutomaton(dfa)).into_stream() {
                    let mut n = 0;
                    while stream.advance() && n < 64 {
                        let key = stream.key();
                        if let (Ok(s), Distance::Exact(d)) = (std::str::from_utf8(key), dfa.eval(key)) {
                            let boost = if d == 0 { 1.0 } else if d == 1 { 0.45 } else { 0.3 };
                            let e = found.entry(s.to_owned()).or_insert(0.0);
                            *e = e.max(boost);
                        }
                        n += 1;
                    }
                }
            }
        }

        let mut ranked: Vec<(String, f32, u64)> = found
            .into_iter()
            .map(|(t, b)| {
                let df = searcher.doc_freq(&Term::from_field_text(field, &t)).unwrap_or(0);
                (t, b, df)
            })
            .filter(|(t, _, df)| *df > 0 || t == word)
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)));
        ranked.truncate(MAX_EXPANSIONS);
        ranked.into_iter().filter(|(_, _, df)| *df > 0).map(|(t, b, _)| (t, b)).collect()
    }

    pub fn search(&self, query: &str, limit: usize) -> SearchResults {
        let start = Instant::now();
        let searcher = self.reader.searcher();
        let Some((query, highlight)) = self.build_query(&searcher, query) else {
            return SearchResults { elapsed: start.elapsed(), ..Default::default() };
        };
        let (top, total) = match searcher.search(&*query, &(TopDocs::with_limit(limit).order_by_score(), Count)) {
            Ok(r) => r,
            Err(e) => {
                log::warn!("search failed: {e}");
                return SearchResults::default();
            }
        };
        let f = &self.fields;
        let mut analyzer = analyzer();
        let hits = top
            .into_iter()
            .filter_map(|(_, addr)| searcher.doc::<TantivyDocument>(addr).ok())
            .map(|doc| {
                let text = |field| doc.get_first(field).and_then(|v| v.as_str()).unwrap_or("").to_owned();
                let num = |field| doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0);
                let name = text(f.name);
                let preview = text(f.preview);
                Hit {
                    path: text(f.path),
                    name: highlight_all(&mut analyzer, &name, &highlight),
                    snippet: snippet(&mut analyzer, &preview, &highlight, SNIPPET_CHARS),
                    mtime: num(f.mtime),
                    size: num(f.size),
                    kind: text(f.kind),
                }
            })
            .collect();
        SearchResults { total, hits, elapsed: start.elapsed() }
    }

    /// Turns user input into a query. Every word must match (in the name, folder or content);
    /// words may match fuzzily, and the word being typed also matches as a prefix.
    /// Text in double quotes is matched as an exact phrase.
    fn build_query(&self, searcher: &Searcher, input: &str) -> Option<(Box<dyn Query>, HashSet<String>)> {
        let f = self.fields;
        let mut analyzer = analyzer();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut highlight = HashSet::new();

        let (words, phrases) = split_query(input);
        let typing_last = !input.ends_with(char::is_whitespace) && !input.trim_end().ends_with('"');

        let mut terms: Vec<String> = Vec::new();
        for w in &words {
            terms.extend(tokenize(&mut analyzer, w));
        }
        for (i, word) in terms.iter().enumerate() {
            let prefix = typing_last && i + 1 == terms.len();
            let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for (field, field_boost) in [(f.name, NAME_BOOST), (f.dir, DIR_BOOST), (f.content, CONTENT_BOOST)] {
                for (text, boost) in self.expand(searcher, field, word, prefix) {
                    let term = Term::from_field_text(field, &text);
                    let q = TermQuery::new(term, IndexRecordOption::WithFreqs);
                    shoulds.push((Occur::Should, Box::new(BoostQuery::new(Box::new(q), boost * field_boost))));
                    highlight.insert(text);
                }
            }
            if shoulds.is_empty() {
                return None; // A word that matches nothing: no document can match all words.
            }
            clauses.push((Occur::Must, Box::new(BooleanQuery::new(shoulds))));
        }

        for phrase in &phrases {
            let tokens = tokenize(&mut analyzer, phrase);
            let q: Box<dyn Query> = match tokens.len() {
                0 => continue,
                1 => {
                    let shoulds: Vec<(Occur, Box<dyn Query>)> = [(f.name, NAME_BOOST), (f.content, CONTENT_BOOST)]
                        .into_iter()
                        .map(|(field, boost)| {
                            let q = TermQuery::new(Term::from_field_text(field, &tokens[0]), IndexRecordOption::WithFreqs);
                            (Occur::Should, Box::new(BoostQuery::new(Box::new(q), boost)) as Box<dyn Query>)
                        })
                        .collect();
                    Box::new(BooleanQuery::new(shoulds))
                }
                _ => {
                    let phrase_for = |field| {
                        let terms = tokens.iter().map(|t| Term::from_field_text(field, t)).collect();
                        Box::new(PhraseQuery::new(terms)) as Box<dyn Query>
                    };
                    Box::new(BooleanQuery::new(vec![
                        (Occur::Should, Box::new(BoostQuery::new(phrase_for(f.name), NAME_BOOST))),
                        (Occur::Should, phrase_for(f.content)),
                    ]))
                }
            };
            highlight.extend(tokens);
            clauses.push((Occur::Must, q));
        }

        if clauses.is_empty() {
            return None;
        }
        Some((Box::new(BooleanQuery::new(clauses)), highlight))
    }
}

/// Adapts a Levenshtein DFA to the term dictionary's automaton interface.
struct DfaAutomaton<'a>(&'a DFA);

impl tantivy_fst::Automaton for DfaAutomaton<'_> {
    type State = u32;

    fn start(&self) -> u32 {
        self.0.initial_state()
    }

    fn is_match(&self, state: &u32) -> bool {
        matches!(self.0.distance(*state), Distance::Exact(_))
    }

    fn can_match(&self, state: &u32) -> bool {
        *state != levenshtein_automata::SINK_STATE
    }

    fn accept(&self, state: &u32, byte: u8) -> u32 {
        self.0.transition(*state, byte)
    }
}

/// Splits input into loose words and "quoted phrases".
fn split_query(input: &str) -> (Vec<String>, Vec<String>) {
    let mut words = Vec::new();
    let mut phrases = Vec::new();
    for (i, part) in input.split('"').enumerate() {
        // Odd parts are inside quotes (an unterminated quote still counts as a phrase).
        if i % 2 == 1 {
            phrases.push(part.to_owned());
        } else {
            words.extend(part.split_whitespace().map(String::from));
        }
    }
    (words, phrases)
}

fn tokenize(analyzer: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stream = analyzer.token_stream(text);
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// Byte ranges of tokens in `text` whose normalized form is in `terms`.
fn matches(analyzer: &mut TextAnalyzer, text: &str, terms: &HashSet<String>) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    let mut stream = analyzer.token_stream(text);
    while stream.advance() {
        let t = stream.token();
        if terms.contains(&t.text) {
            out.push((t.offset_from, t.offset_to, t.text.clone()));
        }
    }
    out
}

fn highlight_all(analyzer: &mut TextAnalyzer, text: &str, terms: &HashSet<String>) -> Vec<Segment> {
    let m = matches(analyzer, text, terms);
    segments(text, 0, text.len(), &m, false)
}

/// Picks the window of `text` with the most distinct matched terms and returns it as segments.
/// Without matches, the start of the text is returned so the user still sees what the file is.
fn snippet(analyzer: &mut TextAnalyzer, text: &str, terms: &HashSet<String>, max_chars: usize) -> Vec<Segment> {
    let mut m = matches(analyzer, text, terms);
    m.truncate(2000);
    let max_bytes = max_chars * 2; // Rough budget; trimmed to `max_chars` characters below.

    let mut best = (0usize, 0usize);
    let mut best_start = 0usize;
    for i in 0..m.len().min(300) {
        let limit = m[i].0 + max_bytes;
        let mut seen: HashSet<&str> = HashSet::new();
        let mut count = 0;
        for x in m[i..].iter().take_while(|x| x.1 <= limit) {
            seen.insert(&x.2);
            count += 1;
        }
        if (seen.len(), count) > best {
            best = (seen.len(), count);
            best_start = i;
        }
    }

    let mut start = m.get(best_start).map(|x| x.0).unwrap_or(0);
    // Show a little context before the first match, starting on a word boundary.
    if start > 0 {
        let context_start = floor_char_boundary(text, start.saturating_sub(60));
        start = match text[context_start..start].find(char::is_whitespace) {
            Some(ws) if context_start > 0 => context_start + ws + 1,
            _ => context_start,
        };
    }
    let end_chars = text[start..].char_indices().nth(max_chars).map(|(i, _)| start + i).unwrap_or(text.len());
    let end = if end_chars < text.len() {
        text[start..end_chars].rfind(char::is_whitespace).map(|i| start + i).filter(|&e| e > start).unwrap_or(end_chars)
    } else {
        end_chars
    };
    let segs = segments(text, start, end, &m, true);
    let mut out: Vec<Segment> = Vec::new();
    if start > 0 {
        out.push(("…".into(), false));
    }
    out.extend(segs);
    if end < text.len() && !out.is_empty() {
        out.push(("…".into(), false));
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn segments(text: &str, start: usize, end: usize, m: &[(usize, usize, String)], collapse: bool) -> Vec<Segment> {
    let norm = |s: &str| -> String {
        if !collapse {
            return s.to_owned();
        }
        let mut out = String::with_capacity(s.len());
        let mut last_ws = false;
        for c in s.chars() {
            if c.is_whitespace() || c.is_control() {
                if !last_ws {
                    out.push(' ');
                }
                last_ws = true;
            } else {
                out.push(c);
                last_ws = false;
            }
        }
        out
    };
    let mut out: Vec<Segment> = Vec::new();
    let mut pos = start;
    for (from, to, _) in m.iter().filter(|x| x.0 >= start && x.1 <= end) {
        if *from > pos {
            out.push((norm(&text[pos..*from]), false));
        }
        out.push((text[*from..*to].to_owned(), true));
        pos = *to;
    }
    if pos < end {
        out.push((norm(&text[pos..end]), false));
    }
    if let Some(first) = out.first_mut() {
        if !first.1 {
            first.0 = first.0.trim_start().to_owned();
        }
    }
    out.retain(|s| !s.0.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_split() {
        let (w, p) = split_query(r#"invoice "net total" 2023"#);
        assert_eq!(w, vec!["invoice", "2023"]);
        assert_eq!(p, vec!["net total"]);
    }

    #[test]
    fn snippet_window() {
        let mut a = analyzer();
        let terms: HashSet<String> = ["budget".into(), "forecast".into()].into();
        let text = format!("{} The budget and the forecast for next year. {}", "lorem ipsum ".repeat(50), "tail ".repeat(80));
        let s = snippet(&mut a, &text, &terms, 120);
        let highlighted: Vec<_> = s.iter().filter(|x| x.1).map(|x| x.0.as_str()).collect();
        assert_eq!(highlighted, vec!["budget", "forecast"]);
        assert_eq!(s.first().unwrap().0, "…");
        let plain: String = s.iter().map(|x| x.0.as_str()).collect();
        assert!(plain.chars().count() <= 124, "{plain}");
    }
}
