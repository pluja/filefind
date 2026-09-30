use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use filefind_core::extract::{extract_guarded, kind_for};
use filefind_core::{
    Category, Engine, Event, Extractor, Failure, Filters, Folder, IndexOptions, SearchRequest, Service, Sort, Status,
};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn helper() -> Extractor {
    Extractor::with_helper(PathBuf::from(env!("CARGO_BIN_EXE_extract-helper")))
}

#[test]
fn extracts_every_format() {
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let kind = kind_for(&path).unwrap_or_else(|| panic!("unsupported fixture {path:?}"));
        let text = extract_guarded(&path, kind).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let expected = if path.file_name().unwrap().to_string_lossy().starts_with("sheet") {
            "Kilimanjaro\t1200"
        } else {
            "photosynthesis"
        };
        assert!(text.contains(expected), "{path:?} gave {text:?}");
    }
}

#[test]
fn helper_processes_isolate_failures() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.pdf");
    std::fs::write(&broken, b"%PDF-1.4 this is not really a pdf").unwrap();

    let extractor = helper();
    let pdf = fixtures().join("sample.pdf");
    for _ in 0..3 {
        // The same helper serves several files, and a bad file doesn't poison it.
        assert!(extractor.extract(&pdf, kind_for(&pdf).unwrap()).unwrap().contains("photosynthesis"));
        let err = extractor.extract(&broken, kind_for(&broken).unwrap()).unwrap_err();
        assert_eq!(err.failure, Failure::Unreadable);
    }

    // A helper that dies without answering is reported as a crash, not a hang.
    let dead = Extractor::with_helper(PathBuf::from("/bin/true"));
    assert_eq!(dead.extract(&pdf, kind_for(&pdf).unwrap()).unwrap_err().failure, Failure::Crashed);
}

struct Harness {
    engine: Arc<Engine>,
    service: Service,
    events: mpsc::Receiver<Event>,
    _index: tempfile::TempDir,
}

impl Harness {
    fn new() -> Harness {
        let index = tempfile::tempdir().unwrap();
        let engine = Arc::new(Engine::open(index.path()).unwrap());
        let (tx, events) = mpsc::channel();
        let service = Service::start(engine.clone(), helper(), move |e| {
            let _ = tx.send(e);
        });
        Harness { engine, service, events, _index: index }
    }

    fn set(&self, folders: Vec<Folder>, options: IndexOptions) -> Status {
        self.service.set_library(folders, options);
        self.idle()
    }

    fn idle(&self) -> Status {
        loop {
            match self.events.recv_timeout(Duration::from_secs(60)).expect("indexer event") {
                Event::Idle(status) => return status,
                Event::Error(e) => panic!("{e}"),
                Event::Progress(_) => {}
            }
        }
    }

    fn names(&self, req: SearchRequest) -> Vec<String> {
        self.engine
            .search(&req)
            .hits
            .into_iter()
            .map(|h| Path::new(&h.path).file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    fn find(&self, query: &str) -> Vec<String> {
        let mut names = self.names(SearchRequest::text(query));
        names.sort();
        names
    }
}

const DOCS: [&str; 6] = ["sample.doc", "sample.docx", "sample.html", "sample.odt", "sample.pdf", "sample.rtf"];

fn library() -> (tempfile::TempDir, PathBuf) {
    let library = tempfile::tempdir().unwrap();
    let docs = library.path().join("Reports 2024");
    std::fs::create_dir(&docs).unwrap();
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        std::fs::copy(&path, docs.join(path.file_name().unwrap())).unwrap();
    }
    std::fs::create_dir(docs.join(".hidden")).unwrap();
    std::fs::write(docs.join(".hidden/secret.txt"), "photosynthesis").unwrap();
    std::fs::write(docs.join("notes.md"), "Groceries: marmalade, bread and cheese").unwrap();
    std::fs::write(docs.join("facturas.txt"), "Las facturas del mes").unwrap();
    std::fs::write(docs.join("holiday photo.jpg"), b"\xff\xd8\xff not really a jpeg").unwrap();
    (library, docs)
}

#[test]
fn searches() {
    let (library, docs) = library();
    let h = Harness::new();
    let status = h.set(vec![Folder::new(library.path().to_owned())], IndexOptions::default());
    assert_eq!(status.docs, 11);
    assert_eq!(status.folders, [11]);
    assert_eq!(status.failed, 0);

    assert_eq!(h.find("photosynthesis"), DOCS);
    // Typos, accents and case don't matter.
    assert_eq!(h.find("fotosynthesis"), DOCS);
    assert_eq!(h.find("ZURICH budget"), DOCS);
    // The word being typed matches as a prefix; a finished word doesn't.
    assert_eq!(h.find("marma").len(), 7);
    assert_eq!(h.find("marma ").len(), 0);
    // Other forms of a word, in English and Spanish.
    assert_eq!(h.find("reports ").len(), 11, "folder name 'Reports' and content 'Report'");
    assert_eq!(h.find("factura "), ["facturas.txt"]);
    assert_eq!(h.find("committees ").len(), 6);

    // Syntax.
    assert_eq!(h.find("\"net total\"").len(), 6);
    assert_eq!(h.find("\"total net\"").len(), 0);
    assert_eq!(h.find("photosynthesis type:pdf"), ["sample.pdf"]);
    assert_eq!(h.find("photosynthesis type:docx"), ["sample.docx"]);
    assert_eq!(h.find("photosynthesis -marmalade").len(), 0);
    assert_eq!(h.find("marmalade -photosynthesis"), ["notes.md"]);
    assert_eq!(h.find("name:sheet"), ["sheet.ods", "sheet.xlsx"]);
    assert_eq!(h.find("groceries in:reports"), ["notes.md"]);
    assert_eq!(h.find("groceries in:taxes").len(), 0);
    assert_eq!(h.find("type:image"), ["holiday photo.jpg"]);

    // Files that aren't documents are found by name.
    assert_eq!(h.find("holiday"), ["holiday photo.jpg"]);

    // Filters and sorting.
    let only = |categories: Vec<Category>| SearchRequest {
        query: "photosynthesis".into(),
        filters: Filters { categories, ..Default::default() },
        ..Default::default()
    };
    assert_eq!(h.names(only(vec![Category::Pdf, Category::Document])).len(), 5);
    let by_name = SearchRequest { query: "photosynthesis".into(), sort: Sort::Name, ..Default::default() };
    assert_eq!(h.names(by_name), DOCS);
    let future = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + 3600;
    let recent = SearchRequest {
        query: "photosynthesis".into(),
        filters: Filters { modified_since: Some(future), ..Default::default() },
        ..Default::default()
    };
    assert!(h.names(recent).is_empty());
    let hits = h.engine.search(&SearchRequest::text("marmalade")).hits;
    assert!(hits.iter().all(|h| h.matches >= 1), "every hit counts its matches");
    std::fs::write(docs.join("notes.md"), "marmalade marmalade Marmalade").unwrap();
    h.idle();
    let notes = h.engine.search(&SearchRequest::text("marmalade")).hits.into_iter().find(|h| h.path.ends_with("notes.md")).unwrap();
    assert_eq!(notes.matches, 3);
    let hit = &h.engine.search(&SearchRequest::text("photosynthesis")).hits[0];
    assert!(hit.snippet.iter().any(|(s, hl)| *hl && s == "photosynthesis"), "{:?}", hit.snippet);

    // Filters alone list files, newest first.
    let pdfs = SearchRequest { filters: Filters { categories: vec![Category::Pdf], ..Default::default() }, ..Default::default() };
    assert_eq!(h.names(pdfs), ["sample.pdf"]);
    let in_folder = SearchRequest {
        filters: Filters { folder: Some(Folder::new(docs.clone())), ..Default::default() },
        ..Default::default()
    };
    assert_eq!(h.names(in_folder).len(), 11);

    // Without file names, only documents are indexed.
    let status = h.set(vec![Folder::new(library.path().to_owned())], IndexOptions { file_names: false, hidden_files: false });
    assert_eq!(status.docs, 10);
    // Hidden files on request.
    let status = h.set(vec![Folder::new(library.path().to_owned())], IndexOptions { file_names: false, hidden_files: true });
    assert_eq!(status.docs, 11);
}

#[test]
fn follows_changes() {
    let (library, docs) = library();
    let h = Harness::new();
    let root = vec![Folder::new(library.path().to_owned())];
    h.set(root.clone(), IndexOptions::default());

    std::fs::write(docs.join("notes.md"), "Groceries: apples").unwrap();
    std::fs::remove_file(docs.join("sample.pdf")).unwrap();
    std::fs::rename(docs.join("sample.rtf"), docs.join("renamed.rtf")).unwrap();
    std::fs::write(docs.join("fresh.txt"), "a brand new photosynthesis note").unwrap();
    std::fs::write(docs.join("broken.pdf"), b"%PDF-1.4 garbage").unwrap();
    let status = h.idle();
    assert_eq!(h.find("marmalade").len(), 5);
    assert_eq!(h.find("photosynthesis"), ["fresh.txt", "renamed.rtf", "sample.doc", "sample.docx", "sample.html", "sample.odt"]);
    assert_eq!(status.failed, 1);
    let failed = h.engine.failed_files(10);
    assert_eq!(failed.len(), 1);
    assert!(failed[0].path.ends_with("broken.pdf"));
    // Unreadable files are still found by name.
    assert_eq!(h.find("broken"), ["broken.pdf"]);

    // Moving a folder out of the library drops everything inside it, and back restores it.
    let outside = tempfile::tempdir().unwrap();
    std::fs::rename(&docs, outside.path().join("moved")).unwrap();
    assert_eq!(h.idle().docs, 0);
    std::fs::rename(outside.path().join("moved"), &docs).unwrap();
    assert_eq!(h.idle().docs, 12);

    // An unchanged rescan does no work and keeps everything.
    h.service.rescan();
    assert_eq!(h.idle().docs, 12);

    h.service.set_library(vec![], IndexOptions::default());
    assert_eq!(h.idle().docs, 0);
    h.service.shutdown();
}

#[test]
fn folder_only_mode() {
    let top = tempfile::tempdir().unwrap();
    std::fs::write(top.path().join("top.txt"), "orchid").unwrap();
    std::fs::create_dir(top.path().join("sub")).unwrap();
    std::fs::write(top.path().join("sub/deep.txt"), "orchid").unwrap();

    let h = Harness::new();
    let flat = Folder { path: top.path().to_owned(), include_subfolders: false };
    let status = h.set(vec![flat.clone()], IndexOptions::default());
    assert_eq!(status.docs, 1);
    assert_eq!(h.find("orchid"), ["top.txt"]);

    // New files directly inside are picked up; new files in subfolders are not.
    std::fs::write(top.path().join("new.txt"), "orchid").unwrap();
    std::fs::write(top.path().join("sub/new-deep.txt"), "orchid").unwrap();
    h.idle();
    assert_eq!(h.find("orchid"), ["new.txt", "top.txt"]);

    let status = h.set(vec![Folder::new(top.path().to_owned())], IndexOptions::default());
    assert_eq!(status.docs, 4);
    assert_eq!(h.engine.count_in(&flat), 2);
}

#[test]
fn shutdown_releases_the_index() {
    let index = tempfile::tempdir().unwrap();
    for _ in 0..2 {
        let engine = Arc::new(Engine::open(index.path()).unwrap());
        let (tx, rx) = mpsc::channel();
        let service = Service::start(engine, Extractor::in_process(), move |e| {
            let _ = tx.send(e);
        });
        service.set_library(vec![], IndexOptions::default());
        assert!(matches!(rx.recv_timeout(Duration::from_secs(30)).unwrap(), Event::Idle(_)));
        // Opening a second writer only works once the first is gone.
        service.shutdown();
    }
}
