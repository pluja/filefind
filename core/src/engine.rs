//! The search index: schema, document construction and querying.

use std::collections::{HashMap, HashSet};
use std::ops::Bound;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use levenshtein_automata::{Distance, LevenshteinAutomatonBuilder, DFA};
use rust_stemmers::{Algorithm, Stemmer};
use tantivy::collector::{Count, TopDocs};
use tantivy::directory::MmapDirectory;
use tantivy::query::{AllQuery, BooleanQuery, BoostQuery, Occur, PhraseQuery, Query, RangeQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, Value, FAST, INDEXED, STORED, STRING,
};
use tantivy::tokenizer::{AsciiFoldingFilter, LowerCaser, RemoveLongFilter, SimpleTokenizer, TextAnalyzer};
use tantivy::postings::Postings;
use tantivy::{DocAddress, DocId, DocSet, Index, IndexReader, IndexWriter, Order, ReloadPolicy, Searcher, TantivyDocument, Term};

use crate::category::{Category, TypeFilter};
use crate::extract::Failure;
use crate::library::Folder;
use crate::query::{self, ParsedQuery};

/// Bump whenever the schema or tokenization changes; the index is rebuilt on mismatch.
const SCHEMA_VERSION: &str = "2";
const TOKENIZER: &str = "ff";
/// Stored text used for result snippets.
const PREVIEW_BYTES: usize = 256 * 1024;
const SNIPPET_CHARS: usize = 220;
const FAILED: &str = "failed";

const NAME_BOOST: f32 = 2.5;
const DIR_BOOST: f32 = 0.8;
const CONTENT_BOOST: f32 = 1.0;
/// Upper bound on how many dictionary terms one query word may expand to, per field.
const MAX_EXPANSIONS: usize = 40;
const STEM_LANGUAGES: [Algorithm; 2] = [Algorithm::English, Algorithm::Spanish];

fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(SimpleTokenizer::default())
        .filter(RemoveLongFilter::limit(48))
        .filter(LowerCaser)
        .filter(AsciiFoldingFilter)
        .build()
}

#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    parent: Field,
    name: Field,
    name_key: Field,
    dir: Field,
    content: Field,
    preview: Field,
    mtime: Field,
    size: Field,
    ext: Field,
    category: Field,
    status: Field,
    failure: Field,
}

fn build_schema() -> (Schema, Fields) {
    let mut b = Schema::builder();
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text = TextOptions::default().set_indexing_options(indexing);
    let fields = Fields {
        path: b.add_text_field("path", STRING | STORED | FAST),
        parent: b.add_text_field("parent", STRING),
        name: b.add_text_field("name", text.clone().set_stored()),
        name_key: b.add_text_field("name_key", STRING | FAST),
        dir: b.add_text_field("dir", text.clone()),
        content: b.add_text_field("content", text),
        preview: b.add_text_field("preview", STORED),
        mtime: b.add_u64_field("mtime", INDEXED | STORED | FAST),
        size: b.add_u64_field("size", STORED | FAST),
        ext: b.add_text_field("ext", STRING | STORED),
        category: b.add_text_field("category", STRING | STORED),
        status: b.add_text_field("status", STRING),
        failure: b.add_text_field("failure", STORED),
    };
    (b.build(), fields)
}

/// Size and modification time; a file whose `FileMeta` is unchanged is not re-read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileMeta {
    pub mtime: u64,
    pub size: u64,
}

/// What was read from a file.
pub enum Content<'a> {
    Text(&'a str),
    /// Only the name is indexed (not a document format).
    NameOnly,
    Failed(Failure),
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
    pub category: Category,
    /// How often the searched words occur in the file's name and content.
    pub matches: u32,
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    pub total: usize,
    pub hits: Vec<Hit>,
    /// Normalized terms that matched, for highlighting elsewhere (e.g. a preview).
    pub terms: Vec<String>,
    pub elapsed: Duration,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sort {
    #[default]
    Relevance,
    Newest,
    Oldest,
    Name,
    Largest,
}

#[derive(Clone, Debug, Default)]
pub struct Filters {
    /// Any of these categories; empty means all.
    pub categories: Vec<Category>,
    pub modified_since: Option<u64>,
    pub folder: Option<Folder>,
}

impl Filters {
    pub fn is_empty(&self) -> bool {
        self.categories.is_empty() && self.modified_since.is_none() && self.folder.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub filters: Filters,
    pub sort: Sort,
    pub limit: usize,
    /// Match other forms of a word ("invoices" finds "invoice").
    pub word_forms: bool,
}

impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest { query: String::new(), filters: Filters::default(), sort: Sort::Relevance, limit: 100, word_forms: true }
    }
}

impl SearchRequest {
    pub fn text(query: &str) -> SearchRequest {
        SearchRequest { query: query.to_owned(), ..Default::default() }
    }

    /// Whether running this request can produce results.
    pub fn is_empty(&self) -> bool {
        query::parse(&self.query).is_empty() && self.filters.is_empty()
    }
}

/// An indexed file that could not be read.
#[derive(Clone, Debug)]
pub struct FailedFile {
    pub path: String,
    pub failure: Failure,
}

pub struct Engine {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    lev: [OnceLock<LevenshteinAutomatonBuilder>; 2],
    stemmers: Vec<Stemmer>,
}

#[derive(Clone, Copy)]
struct Expansion {
    prefix: bool,
    fuzzy: bool,
    word_forms: bool,
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
        Ok(Engine {
            index,
            reader,
            fields,
            lev: [OnceLock::new(), OnceLock::new()],
            stemmers: STEM_LANGUAGES.into_iter().map(Stemmer::create).collect(),
        })
    }

    /// A writer sized to the machine: generous enough to index quickly, bounded so it
    /// never takes a noticeable share of memory.
    pub fn writer(&self) -> tantivy::Result<IndexWriter> {
        let ram = total_memory().unwrap_or(4 << 30);
        let budget = (ram / 48).clamp(64 << 20, 384 << 20) as usize;
        let cpus = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads = (cpus / 4).clamp(1, 4).min(budget / (24 << 20));
        self.index.writer_with_num_threads(threads, budget)
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

    /// Matches every document located below directory `dir` (at any depth).
    fn under_query(&self, dir: &Path) -> RangeQuery {
        let dir = dir.to_string_lossy();
        let dir = dir.trim_end_matches('/');
        // '0' is the character right after '/', so this covers exactly "dir/...".
        let lower = Term::from_field_text(self.fields.path, &format!("{dir}/"));
        let upper = Term::from_field_text(self.fields.path, &format!("{dir}0"));
        RangeQuery::new(Bound::Included(lower), Bound::Excluded(upper))
    }

    fn folder_query(&self, folder: &Folder) -> Box<dyn Query> {
        if folder.include_subfolders {
            Box::new(self.under_query(&folder.path))
        } else {
            let term = Term::from_field_text(self.fields.parent, &folder.path.to_string_lossy());
            Box::new(TermQuery::new(term, IndexRecordOption::Basic))
        }
    }

    pub fn delete_under(&self, writer: &IndexWriter, dir: &Path) {
        if let Err(e) = writer.delete_query(Box::new(self.under_query(dir))) {
            log::warn!("delete under {}: {e}", dir.display());
        }
    }

    fn count(&self, query: &dyn Query) -> u64 {
        self.reader.searcher().search(query, &Count).unwrap_or(0) as u64
    }

    pub fn count_in(&self, folder: &Folder) -> u64 {
        self.count(&*self.folder_query(folder))
    }

    fn failed_query(&self) -> TermQuery {
        TermQuery::new(Term::from_field_text(self.fields.status, FAILED), IndexRecordOption::Basic)
    }

    pub fn failed_count(&self) -> u64 {
        self.count(&self.failed_query())
    }

    pub fn failed_files(&self, limit: usize) -> Vec<FailedFile> {
        let searcher = self.reader.searcher();
        let Ok(top) = searcher.search(&self.failed_query(), &TopDocs::with_limit(limit).order_by_string_fast_field("path", Order::Asc)) else {
            return Vec::new();
        };
        top.into_iter()
            .filter_map(|(_, addr)| searcher.doc::<TantivyDocument>(addr).ok())
            .map(|doc| FailedFile {
                path: stored_str(&doc, self.fields.path).to_owned(),
                failure: Failure::from_id(stored_str(&doc, self.fields.failure)),
            })
            .collect()
    }

    /// The metadata `path` was indexed with, if it is indexed.
    pub fn indexed_meta(&self, path: &Path) -> Option<FileMeta> {
        let searcher = self.reader.searcher();
        let query = TermQuery::new(self.path_term(&path.to_string_lossy()), IndexRecordOption::Basic);
        let (_, addr) = searcher.search(&query, &TopDocs::with_limit(1).order_by_score()).ok()?.into_iter().next()?;
        let ff = searcher.segment_reader(addr.segment_ord).fast_fields();
        Some(FileMeta { mtime: ff.u64("mtime").ok()?.first(addr.doc_id)?, size: ff.u64("size").ok()?.first(addr.doc_id)? })
    }

    /// All indexed paths with the metadata they were indexed with.
    pub fn existing(&self) -> HashMap<String, FileMeta> {
        let searcher = self.reader.searcher();
        let mut out = HashMap::new();
        for segment in searcher.segment_readers() {
            let ff = segment.fast_fields();
            let (Ok(Some(paths)), Ok(mtimes), Ok(sizes)) = (ff.str("path"), ff.u64("mtime"), ff.u64("size")) else {
                continue;
            };
            let mut path = String::new();
            for doc in segment.doc_ids_alive() {
                let Some(ord) = paths.term_ords(doc).next() else { continue };
                path.clear();
                if paths.ord_to_str(ord, &mut path).unwrap_or(false) {
                    let meta = FileMeta { mtime: mtimes.first(doc).unwrap_or(0), size: sizes.first(doc).unwrap_or(0) };
                    out.insert(path.clone(), meta);
                }
            }
        }
        out
    }

    /// Builds the index document for a file inside library folder `root`.
    pub fn make_doc(&self, path: &Path, root: &Path, meta: FileMeta, content: Content) -> TantivyDocument {
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.path, path.to_string_lossy());
        if let Some(parent) = path.parent() {
            doc.add_text(f.parent, parent.to_string_lossy());
        }
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        doc.add_text(f.name, &name);
        doc.add_text(f.name_key, name.to_lowercase());
        // Folder names are searchable too ("taxes 2023"), relative to the library folder.
        let mut dir_text = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if let Some(rel) = path.parent().and_then(|p| p.strip_prefix(root).ok()) {
            dir_text.push(' ');
            dir_text.push_str(&rel.to_string_lossy());
        }
        doc.add_text(f.dir, &dir_text);
        match content {
            Content::Text(text) => {
                doc.add_text(f.content, text);
                let mut preview = text.to_owned();
                crate::extract::truncate_at_char_boundary(&mut preview, PREVIEW_BYTES);
                doc.add_text(f.preview, &preview);
            }
            Content::NameOnly => {}
            Content::Failed(failure) => {
                doc.add_text(f.status, FAILED);
                doc.add_text(f.failure, failure.id());
            }
        }
        doc.add_u64(f.mtime, meta.mtime);
        doc.add_u64(f.size, meta.size);
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        doc.add_text(f.category, Category::from_extension(&ext).id());
        doc.add_text(f.ext, &ext);
        doc
    }

    fn lev(&self, distance: u8) -> &LevenshteinAutomatonBuilder {
        self.lev[distance as usize - 1].get_or_init(|| LevenshteinAutomatonBuilder::new(distance, true))
    }

    /// Finds dictionary terms in `field` matching `word` exactly, by prefix, as another form
    /// of the same word, or within a small edit distance. Returns term texts with a score
    /// multiplier.
    fn expand(&self, searcher: &Searcher, field: Field, word: &str, how: Expansion) -> Vec<(String, f32)> {
        let mut found: HashMap<String, f32> = HashMap::new();
        found.insert(word.to_owned(), 1.0);
        let mut add = |term: &[u8], boost: f32| {
            if let Ok(s) = std::str::from_utf8(term) {
                let e = found.entry(s.to_owned()).or_insert(0.0);
                *e = e.max(boost);
            }
        };

        let len = word.chars().count();
        let numeric = word.chars().all(|c| c.is_ascii_digit());
        let distance = match len {
            _ if numeric || !how.fuzzy => 0,
            0..=3 => 0,
            4..=7 => 1,
            _ => 2,
        };
        let dfa: Option<DFA> = (distance > 0).then(|| self.lev(distance).build_dfa(word));
        // Stems and the prefix they share with the word, e.g. "invoices" -> ("invoic", "invoic").
        let stems: Vec<(String, String)> = if how.word_forms && len >= 4 && !numeric {
            self.stemmers
                .iter()
                .map(|s| s.stem(word).into_owned())
                .filter_map(|stem| {
                    let shared: String = word.chars().zip(stem.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect();
                    (shared.chars().count() >= 3).then_some((stem, shared))
                })
                .collect()
        } else {
            Vec::new()
        };

        for segment in searcher.segment_readers() {
            let Ok(inverted) = segment.inverted_index(field) else { continue };
            let terms = inverted.terms();
            let scan_prefix = |prefix: &str, limit: usize, f: &mut dyn FnMut(&[u8])| {
                if let Ok(mut stream) = terms.range().ge(prefix.as_bytes()).into_stream() {
                    let mut n = 0;
                    while n < limit && stream.advance() && stream.key().starts_with(prefix.as_bytes()) {
                        f(stream.key());
                        n += 1;
                    }
                }
            };
            if how.prefix && len >= 2 {
                scan_prefix(word, 64, &mut |key| add(key, 0.6));
            }
            for (stem, shared) in &stems {
                let stemmer_forms = |key: &[u8]| {
                    let Ok(term) = std::str::from_utf8(key) else { return false };
                    term.len() <= word.len() + 6 && self.stemmers.iter().any(|s| s.stem(term) == stem.as_str())
                };
                scan_prefix(shared, 400, &mut |key| {
                    if stemmer_forms(key) {
                        add(key, 0.8);
                    }
                });
            }
            if let Some(dfa) = &dfa {
                if let Ok(mut stream) = terms.search(DfaAutomaton(dfa)).into_stream() {
                    let mut n = 0;
                    while n < 64 && stream.advance() {
                        if let Distance::Exact(d) = dfa.eval(stream.key()) {
                            add(stream.key(), if d == 0 { 1.0 } else if d == 1 { 0.45 } else { 0.3 });
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
            .filter(|(_, _, df)| *df > 0)
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)));
        ranked.truncate(MAX_EXPANSIONS);
        ranked.into_iter().map(|(t, b, _)| (t, b)).collect()
    }

    /// A clause matching `word` in any of `fields`, or `None` if no indexed term matches.
    fn word_clause(
        &self,
        searcher: &Searcher,
        fields: &[(Field, f32)],
        word: &str,
        how: Expansion,
        highlight: &mut HashSet<String>,
    ) -> Option<Box<dyn Query>> {
        let mut shoulds: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for &(field, field_boost) in fields {
            for (text, boost) in self.expand(searcher, field, word, how) {
                let q = TermQuery::new(Term::from_field_text(field, &text), IndexRecordOption::WithFreqs);
                shoulds.push((Occur::Should, Box::new(BoostQuery::new(Box::new(q), boost * field_boost))));
                highlight.insert(text);
            }
        }
        (!shoulds.is_empty()).then(|| Box::new(BooleanQuery::new(shoulds)) as Box<dyn Query>)
    }

    fn phrase_clause(&self, tokens: &[String], fields: &[(Field, f32)]) -> Box<dyn Query> {
        let shoulds = fields
            .iter()
            .map(|&(field, boost)| {
                let q: Box<dyn Query> = if tokens.len() == 1 {
                    Box::new(TermQuery::new(Term::from_field_text(field, &tokens[0]), IndexRecordOption::WithFreqs))
                } else {
                    Box::new(PhraseQuery::new(tokens.iter().map(|t| Term::from_field_text(field, t)).collect()))
                };
                (Occur::Should, Box::new(BoostQuery::new(q, boost)) as Box<dyn Query>)
            })
            .collect();
        Box::new(BooleanQuery::new(shoulds))
    }

    fn type_clause(&self, types: &[TypeFilter]) -> Box<dyn Query> {
        let f = self.fields;
        let shoulds = types
            .iter()
            .map(|t| {
                let term = match t {
                    TypeFilter::Category(c) => Term::from_field_text(f.category, c.id()),
                    TypeFilter::Extension(e) => Term::from_field_text(f.ext, e),
                };
                (Occur::Should, Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>)
            })
            .collect();
        Box::new(BooleanQuery::new(shoulds))
    }

    /// Turns a request into a query. Every word must match (in the name, folder or content);
    /// words may match fuzzily or in another form, and the word being typed also matches as
    /// a prefix. Returns `None` when nothing can match.
    fn build_query(&self, searcher: &Searcher, req: &SearchRequest) -> Option<(Box<dyn Query>, HashSet<String>)> {
        let f = self.fields;
        let parsed: ParsedQuery = query::parse(&req.query);
        if parsed.is_empty() && req.filters.is_empty() {
            return None;
        }
        let mut analyzer = analyzer();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut highlight = HashSet::new();
        let all_fields = [(f.name, NAME_BOOST), (f.dir, DIR_BOOST), (f.content, CONTENT_BOOST)];
        let text_fields = [(f.name, NAME_BOOST), (f.content, CONTENT_BOOST)];

        let words: Vec<String> = parsed.words.iter().flat_map(|w| tokenize(&mut analyzer, w)).collect();
        for (i, word) in words.iter().enumerate() {
            let how = Expansion { prefix: parsed.typing_last && i + 1 == words.len(), fuzzy: true, word_forms: req.word_forms };
            clauses.push((Occur::Must, self.word_clause(searcher, &all_fields, word, how, &mut highlight)?));
        }
        let strict = Expansion { prefix: true, fuzzy: true, word_forms: req.word_forms };
        for word in parsed.name_words.iter().flat_map(|w| tokenize(&mut analyzer, w)) {
            clauses.push((Occur::Must, self.word_clause(searcher, &[(f.name, NAME_BOOST)], &word, strict, &mut highlight)?));
        }
        for word in parsed.folders.iter().flat_map(|w| tokenize(&mut analyzer, w)) {
            let mut ignore = HashSet::new();
            let folder = Expansion { prefix: true, fuzzy: true, word_forms: false };
            clauses.push((Occur::Must, self.word_clause(searcher, &[(f.dir, DIR_BOOST)], &word, folder, &mut ignore)?));
        }
        for phrase in &parsed.phrases {
            let tokens = tokenize(&mut analyzer, phrase);
            if !tokens.is_empty() {
                clauses.push((Occur::Must, self.phrase_clause(&tokens, &text_fields)));
                highlight.extend(tokens);
            }
        }
        for excluded in &parsed.excluded {
            let tokens = tokenize(&mut analyzer, excluded);
            if !tokens.is_empty() {
                clauses.push((Occur::MustNot, self.phrase_clause(&tokens, &all_fields)));
            }
        }
        if !parsed.types.is_empty() {
            clauses.push((Occur::Must, self.type_clause(&parsed.types)));
        }
        let filters = &req.filters;
        if !filters.categories.is_empty() {
            let types: Vec<TypeFilter> = filters.categories.iter().map(|c| TypeFilter::Category(*c)).collect();
            clauses.push((Occur::Must, self.type_clause(&types)));
        }
        if let Some(since) = filters.modified_since {
            let range = RangeQuery::new(Bound::Included(Term::from_field_u64(f.mtime, since)), Bound::Unbounded);
            clauses.push((Occur::Must, Box::new(range)));
        }
        if let Some(folder) = &filters.folder {
            clauses.push((Occur::Must, self.folder_query(folder)));
        }
        if !clauses.iter().any(|(occur, _)| *occur == Occur::Must) {
            clauses.push((Occur::Must, Box::new(AllQuery)));
        }
        Some((Box::new(BooleanQuery::new(clauses)), highlight))
    }

    pub fn search(&self, req: &SearchRequest) -> SearchResults {
        let start = Instant::now();
        let searcher = self.reader.searcher();
        let Some((query, highlight)) = self.build_query(&searcher, req) else {
            return SearchResults { elapsed: start.elapsed(), ..Default::default() };
        };
        // Without words to rank by, every match scores the same: show the newest first.
        let sort = match req.sort {
            Sort::Relevance if !query::parse(&req.query).has_terms() => Sort::Newest,
            sort => sort,
        };
        let top = TopDocs::with_limit(req.limit.max(1));
        let found: tantivy::Result<(Vec<DocAddress>, usize)> = match sort {
            Sort::Relevance => searcher.search(&*query, &(top.order_by_score(), Count)).map(|(d, n)| (addresses(d), n)),
            Sort::Newest => searcher.search(&*query, &(top.order_by_u64_field("mtime", Order::Desc), Count)).map(|(d, n)| (addresses(d), n)),
            Sort::Oldest => searcher.search(&*query, &(top.order_by_u64_field("mtime", Order::Asc), Count)).map(|(d, n)| (addresses(d), n)),
            Sort::Largest => searcher.search(&*query, &(top.order_by_u64_field("size", Order::Desc), Count)).map(|(d, n)| (addresses(d), n)),
            Sort::Name => searcher
                .search(&*query, &(top.order_by_string_fast_field("name_key", Order::Asc), Count))
                .map(|(d, n)| (addresses(d), n)),
        };
        let (docs, total) = match found {
            Ok(r) => r,
            Err(e) => {
                log::warn!("search failed: {e}");
                return SearchResults::default();
            }
        };
        let f = &self.fields;
        let mut analyzer = analyzer();
        let counts = self.match_counts(&searcher, &docs, &highlight);
        let hits = docs
            .into_iter()
            .zip(counts)
            .filter_map(|(addr, matches)| Some((searcher.doc::<TantivyDocument>(addr).ok()?, matches)))
            .map(|(doc, matches)| {
                let num = |field| doc.get_first(field).and_then(|v| v.as_u64()).unwrap_or(0);
                Hit {
                    path: stored_str(&doc, f.path).to_owned(),
                    name: highlight_all(&mut analyzer, stored_str(&doc, f.name), &highlight),
                    snippet: snippet(&mut analyzer, stored_str(&doc, f.preview), &highlight, SNIPPET_CHARS),
                    mtime: num(f.mtime),
                    size: num(f.size),
                    category: Category::from_id(stored_str(&doc, f.category)).unwrap_or(Category::Other),
                    matches,
                }
            })
            .collect();
        SearchResults { total, hits, terms: highlight.into_iter().collect(), elapsed: start.elapsed() }
    }
}

impl Engine {
    /// Occurrences of `terms` in each document's name and content, read from the index's
    /// term frequencies (exact even for text longer than the stored preview).
    fn match_counts(&self, searcher: &Searcher, docs: &[DocAddress], terms: &HashSet<String>) -> Vec<u32> {
        let mut counts = vec![0u32; docs.len()];
        let mut by_segment: HashMap<u32, Vec<(DocId, usize)>> = HashMap::new();
        for (i, addr) in docs.iter().enumerate() {
            by_segment.entry(addr.segment_ord).or_default().push((addr.doc_id, i));
        }
        for (segment, mut wanted) in by_segment {
            wanted.sort_unstable();
            let reader = searcher.segment_reader(segment);
            for field in [self.fields.name, self.fields.content] {
                let Ok(inverted) = reader.inverted_index(field) else { continue };
                for term in terms {
                    let term = Term::from_field_text(field, term);
                    let Ok(Some(mut postings)) = inverted.read_postings(&term, IndexRecordOption::WithFreqs) else { continue };
                    for &(doc, i) in &wanted {
                        // Seeking requires ascending targets, hence the sort above.
                        if postings.doc() <= doc && postings.seek(doc) == doc {
                            counts[i] += postings.term_freq();
                        }
                    }
                }
            }
        }
        counts
    }
}

fn addresses<K>(docs: Vec<(K, DocAddress)>) -> Vec<DocAddress> {
    docs.into_iter().map(|(_, addr)| addr).collect()
}

fn stored_str(doc: &TantivyDocument, field: Field) -> &str {
    doc.get_first(field).and_then(|v| v.as_str()).unwrap_or("")
}

pub(crate) fn total_memory() -> Option<u64> {
    let info = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = info.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
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

/// Byte ranges of `text` to highlight for the given normalized terms.
pub fn highlight_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    let terms: HashSet<String> = terms.iter().cloned().collect();
    matches(&mut analyzer(), text, &terms).into_iter().map(|(a, b, _)| (a, b)).collect()
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

    #[test]
    fn highlight_ranges_are_byte_offsets() {
        let text = "Café budget, Budget!";
        let ranges = highlight_ranges(text, &["budget".into()]);
        assert_eq!(ranges.iter().map(|&(a, b)| &text[a..b]).collect::<Vec<_>>(), ["budget", "Budget"]);
    }
}
