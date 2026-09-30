//! Background indexing: keeps the index in sync with the library folders.
//!
//! A single worker thread owns the index writer. It reconciles the index with
//! the file system on startup and whenever the library changes, then follows
//! file changes through inotify, with a periodic rescan as a safety net.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use rayon::prelude::*;
use tantivy::IndexWriter;

use crate::engine::{Engine, FileMeta};
use crate::extract::{self, Extractor};

const CHUNK: usize = 32;
const COMMIT_EVERY: Duration = Duration::from_secs(3);
const DEBOUNCE: Duration = Duration::from_millis(1500);
const RESCAN_EVERY: Duration = Duration::from_secs(30 * 60);
const SKIP_DIRS: &[&str] = &["node_modules", "__pycache__", "site-packages", "lost+found"];

#[derive(Debug, Clone)]
pub enum Event {
    /// Indexing is under way; `done` of `total` changed files processed.
    Indexing { done: usize, total: usize },
    /// The index is up to date and holds `docs` documents.
    Idle { docs: u64 },
    Error(String),
}

enum Cmd {
    Folders(Vec<PathBuf>),
    Rescan,
    Rebuild,
    Changed(Vec<PathBuf>),
}

pub struct Service {
    tx: Sender<Cmd>,
}

impl Service {
    pub fn start(
        engine: Arc<Engine>,
        extractor: Extractor,
        on_event: impl Fn(Event) + Send + 'static,
    ) -> Service {
        let (tx, rx) = mpsc::channel();
        let watch_tx = tx.clone();
        std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || {
                let writer = match engine.writer() {
                    Ok(w) => w,
                    Err(e) => {
                        on_event(Event::Error(format!("Could not open the index: {e}")));
                        return;
                    }
                };
                let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                    if let Ok(ev) = res {
                        let relevant = matches!(
                            ev.kind,
                            EventKind::Create(_) | EventKind::Remove(_) | EventKind::Modify(notify::event::ModifyKind::Data(_))
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                                | EventKind::Modify(notify::event::ModifyKind::Any)
                        );
                        if relevant && !ev.paths.is_empty() {
                            let _ = watch_tx.send(Cmd::Changed(ev.paths));
                        }
                    }
                })
                .map_err(|e| log::warn!("file watching unavailable: {e}"))
                .ok();
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads((std::thread::available_parallelism().map_or(2, |n| n.get()) / 2).clamp(1, 6))
                    .thread_name(|i| format!("extract-{i}"))
                    .start_handler(|_| lower_thread_priority())
                    .build()
                    .expect("thread pool");
                lower_thread_priority();
                let mut worker = Worker {
                    engine,
                    extractor,
                    writer,
                    rx,
                    pool,
                    watcher,
                    watched: HashSet::new(),
                    watch_full: false,
                    roots: Vec::new(),
                    pending: VecDeque::new(),
                    on_event: Box::new(on_event),
                };
                worker.run();
            })
            .expect("spawn indexer");
        Service { tx }
    }

    /// Sets the library folders and reconciles the index with them.
    pub fn set_folders(&self, folders: Vec<PathBuf>) {
        let _ = self.tx.send(Cmd::Folders(folders));
    }

    pub fn rescan(&self) {
        let _ = self.tx.send(Cmd::Rescan);
    }

    /// Drops the whole index and indexes everything again.
    pub fn rebuild(&self) {
        let _ = self.tx.send(Cmd::Rebuild);
    }
}

fn lower_thread_priority() {
    // Indexing is background work: never compete with the user's foreground apps.
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, libc::gettid() as libc::id_t, 10);
    }
}

struct Worker {
    engine: Arc<Engine>,
    extractor: Extractor,
    writer: IndexWriter,
    rx: Receiver<Cmd>,
    pool: rayon::ThreadPool,
    watcher: Option<RecommendedWatcher>,
    watched: HashSet<PathBuf>,
    watch_full: bool,
    roots: Vec<PathBuf>,
    pending: VecDeque<Cmd>,
    on_event: Box<dyn Fn(Event) + Send>,
}

/// A file found on disk that needs (re)indexing.
struct Candidate {
    path: PathBuf,
    root: usize,
    kind: extract::Kind,
    meta: FileMeta,
}

enum SyncOutcome {
    Done,
    Interrupted,
}

impl Worker {
    fn run(&mut self) {
        loop {
            let cmd = match self.pending.pop_front() {
                Some(cmd) => cmd,
                None => match self.rx.recv_timeout(RESCAN_EVERY) {
                    Ok(cmd) => cmd,
                    Err(RecvTimeoutError::Timeout) => Cmd::Rescan,
                    Err(RecvTimeoutError::Disconnected) => return,
                },
            };
            match cmd {
                Cmd::Folders(roots) => {
                    self.roots = roots;
                    self.sync();
                }
                Cmd::Rescan => self.sync(),
                Cmd::Rebuild => {
                    let _ = self.writer.delete_all_documents();
                    self.commit();
                    self.sync();
                }
                Cmd::Changed(paths) => self.changed(paths),
            }
        }
    }

    fn emit_idle(&self) {
        (self.on_event)(Event::Idle { docs: self.engine.num_docs() });
    }

    fn commit(&mut self) {
        if let Err(e) = self.writer.commit() {
            log::error!("commit failed: {e}");
            (self.on_event)(Event::Error(format!("Could not save the index: {e}")));
        }
        self.engine.reload();
    }

    /// Checks for new commands without blocking. Returns true if the current
    /// full sync should stop because a newer one supersedes it.
    fn poll_interrupt(&mut self) -> bool {
        while let Ok(cmd) = self.rx.try_recv() {
            self.pending.push_back(cmd);
        }
        self.pending.iter().any(|c| matches!(c, Cmd::Folders(_) | Cmd::Rebuild | Cmd::Rescan))
    }

    fn sync(&mut self) {
        let outcome = self.sync_inner();
        self.commit();
        if matches!(outcome, SyncOutcome::Done) {
            self.emit_idle();
        }
    }

    fn sync_inner(&mut self) -> SyncOutcome {
        let existing = self.engine.existing();
        let offline: Vec<&PathBuf> = self.roots.iter().filter(|r| !r.is_dir()).collect();

        let mut seen: HashSet<String> = HashSet::new();
        let mut todo: Vec<Candidate> = Vec::new();
        let mut dirs: HashSet<PathBuf> = HashSet::new();
        for (root_idx, root) in self.roots.iter().enumerate() {
            walk(root, &mut dirs, |path, kind, meta| {
                let key = path.to_string_lossy().into_owned();
                if existing.get(&key) != Some(&meta.mtime) {
                    todo.push(Candidate { path: path.to_owned(), root: root_idx, kind, meta });
                }
                seen.insert(key);
            });
        }

        // Forget files that disappeared, but keep those on folders that are
        // currently unavailable (an unplugged drive) until it comes back.
        let mut removed = 0;
        for path in existing.keys() {
            if !seen.contains(path) && !offline.iter().any(|r| Path::new(path).starts_with(r)) {
                self.writer.delete_term(self.engine.path_term(path));
                removed += 1;
            }
        }
        if removed > 0 {
            self.commit();
        }
        self.update_watches(dirs);

        // Small files first: most of the library becomes searchable quickly.
        todo.sort_by_key(|c| c.meta.size);
        let total = todo.len();
        let mut done = 0;
        let mut last_commit = Instant::now();
        if total > 0 {
            (self.on_event)(Event::Indexing { done, total });
        }
        for chunk in todo.chunks(CHUNK) {
            if self.poll_interrupt() {
                return SyncOutcome::Interrupted;
            }
            self.index_files(chunk);
            done += chunk.len();
            (self.on_event)(Event::Indexing { done, total });
            if last_commit.elapsed() > COMMIT_EVERY {
                self.commit();
                last_commit = Instant::now();
            }
        }
        SyncOutcome::Done
    }

    fn index_files(&mut self, files: &[Candidate]) {
        let extractor = &self.extractor;
        let engine = &self.engine;
        let roots = &self.roots;
        let docs: Vec<_> = self.pool.install(|| {
            files
                .par_iter()
                .map(|c| {
                    // Files that fail to parse are still indexed by name, and not retried
                    // until they change.
                    let text = extractor.extract(&c.path, c.kind).unwrap_or_else(|e| {
                        log::info!("{}: {e}", c.path.display());
                        String::new()
                    });
                    let root = roots.get(c.root).map(PathBuf::as_path).unwrap_or(Path::new("/"));
                    (c.path.to_string_lossy().into_owned(), engine.make_doc(&c.path, root, &c.meta, &text))
                })
                .collect()
        });
        for (path, doc) in docs {
            self.writer.delete_term(self.engine.path_term(&path));
            if let Err(e) = self.writer.add_document(doc) {
                log::error!("add {path}: {e}");
            }
        }
    }

    fn update_watches(&mut self, dirs: HashSet<PathBuf>) {
        let Some(watcher) = self.watcher.as_mut() else { return };
        for gone in self.watched.difference(&dirs) {
            let _ = watcher.unwatch(gone);
        }
        self.watched.retain(|d| dirs.contains(d));
        for dir in dirs {
            if self.watch_full {
                break;
            }
            if self.watched.contains(&dir) {
                continue;
            }
            match watcher.watch(&dir, RecursiveMode::NonRecursive) {
                Ok(()) => {
                    self.watched.insert(dir);
                }
                Err(e) if matches!(e.kind, notify::ErrorKind::MaxFilesWatch) => {
                    log::warn!("inotify watch limit reached; relying on periodic rescans");
                    self.watch_full = true;
                }
                Err(_) => {}
            }
        }
    }

    fn root_index(&self, path: &Path) -> Option<usize> {
        self.roots.iter().position(|r| path.starts_with(r))
    }

    /// Applies file system changes, after waiting briefly for related events to settle.
    fn changed(&mut self, first: Vec<PathBuf>) {
        let mut paths: HashSet<PathBuf> = first.into_iter().collect();
        let deadline = Instant::now() + DEBOUNCE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.rx.recv_timeout(left) {
                Ok(Cmd::Changed(more)) => paths.extend(more),
                Ok(other) => self.pending.push_back(other),
                Err(_) => break,
            }
        }

        let mut todo = Vec::new();
        let mut new_dirs = HashSet::new();
        for path in paths {
            let Some(root_idx) = self.root_index(&path) else { continue };
            let root = &self.roots[root_idx];
            if path.strip_prefix(root).is_ok_and(|rel| rel.components().any(|c| is_skipped(c.as_os_str()))) {
                continue;
            }
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.is_dir() => {
                    walk(&path, &mut new_dirs, |p, kind, meta| {
                        todo.push(Candidate { path: p.to_owned(), root: root_idx, kind, meta });
                    });
                }
                Ok(m) if m.is_file() => {
                    if let Some(c) = candidate(&path, &m) {
                        todo.push(Candidate { root: root_idx, ..c });
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    let key = path.to_string_lossy();
                    self.writer.delete_term(self.engine.path_term(&key));
                    self.engine.delete_under(&self.writer, &key);
                    if self.watched.remove(&path) {
                        if let Some(w) = self.watcher.as_mut() {
                            let _ = w.unwatch(&path);
                        }
                    }
                }
            }
        }
        if !new_dirs.is_empty() {
            let mut all = self.watched.clone();
            all.extend(new_dirs);
            self.update_watches(all);
        }
        for chunk in todo.chunks(CHUNK) {
            self.index_files(chunk);
        }
        self.commit();
        if !self.pending.iter().any(|c| matches!(c, Cmd::Folders(_) | Cmd::Rebuild | Cmd::Rescan)) {
            self.emit_idle();
        }
    }
}

fn is_skipped(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref())
}

fn candidate(path: &Path, m: &std::fs::Metadata) -> Option<Candidate> {
    let kind = extract::kind_for(path)?;
    if !extract::is_indexable(kind, m.len()) {
        return None;
    }
    let mtime = m.modified().ok()?.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    Some(Candidate { path: path.to_owned(), root: 0, kind, meta: FileMeta { mtime, size: m.len() } })
}

/// Walks `dir`, skipping hidden entries, dependency folders and cache directories.
/// Records visited directories in `dirs` and reports indexable files.
fn walk(dir: &Path, dirs: &mut HashSet<PathBuf>, mut found: impl FnMut(&Path, extract::Kind, FileMeta)) {
    let walker = walkdir::WalkDir::new(dir).follow_links(false).into_iter().filter_entry(|e| {
        if e.depth() == 0 {
            return true;
        }
        if is_skipped(e.file_name()) {
            return false;
        }
        // Directories tagged as caches (CACHEDIR.TAG) hold nothing worth searching.
        !(e.file_type().is_dir() && e.path().join("CACHEDIR.TAG").exists())
    });
    for entry in walker.filter_map(Result::ok) {
        let ft = entry.file_type();
        if ft.is_dir() {
            dirs.insert(entry.into_path());
        } else if ft.is_file() {
            if let Ok(m) = entry.metadata() {
                if let Some(c) = candidate(entry.path(), &m) {
                    found(&c.path, c.kind, c.meta);
                }
            }
        }
    }
}
