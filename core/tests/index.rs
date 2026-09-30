use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

use filefind_core::extract::{extract_guarded, kind_for};
use filefind_core::{Engine, Event, Extractor, Service};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

#[test]
fn extracts_every_format() {
    for entry in std::fs::read_dir(fixtures()).unwrap() {
        let path = entry.unwrap().path();
        let kind = kind_for(&path).unwrap_or_else(|| panic!("unsupported fixture {path:?}"));
        let text = extract_guarded(&path, kind).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        let expected = if path.file_name().unwrap().to_string_lossy().starts_with("sheet") {
            "Kilimanjaro"
        } else {
            "photosynthesis"
        };
        assert!(text.contains(expected), "{path:?} gave {text:?}");
    }
}

fn wait_idle(rx: &mpsc::Receiver<Event>) -> u64 {
    loop {
        match rx.recv_timeout(Duration::from_secs(60)).expect("indexer event") {
            Event::Idle { docs } => return docs,
            Event::Error(e) => panic!("{e}"),
            Event::Indexing { .. } => {}
        }
    }
}

fn names(engine: &Engine, query: &str) -> Vec<String> {
    let mut names: Vec<String> = engine
        .search(query, 50)
        .hits
        .into_iter()
        .map(|h| Path::new(&h.path).file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn indexes_and_searches() {
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

    let index_dir = tempfile::tempdir().unwrap();
    let engine = Arc::new(Engine::open(index_dir.path()).unwrap());
    let (tx, rx) = mpsc::channel();
    let service = Service::start(engine.clone(), Extractor::default(), move |e| {
        let _ = tx.send(e);
    });
    service.set_folders(vec![library.path().to_owned()]);
    assert_eq!(wait_idle(&rx), 9);

    let docs_with_text = ["sample.doc", "sample.docx", "sample.html", "sample.odt", "sample.pdf", "sample.rtf"];
    assert_eq!(names(&engine, "photosynthesis"), docs_with_text);
    // Typos, accents and case don't matter.
    assert_eq!(names(&engine, "fotosynthesis"), docs_with_text);
    assert_eq!(names(&engine, "ZURICH budget"), docs_with_text);
    // The word being typed matches as a prefix.
    assert_eq!(names(&engine, "marma").len(), 7);
    assert_eq!(names(&engine, "marma ").len(), 0);
    // Phrases, file names and folder names.
    assert_eq!(names(&engine, "\"net total\"").len(), 6);
    assert_eq!(names(&engine, "\"total net\"").len(), 0);
    assert_eq!(names(&engine, "sheet kilimanjaro"), ["sheet.ods", "sheet.xlsx"]);
    assert_eq!(names(&engine, "reports groceries"), ["notes.md"]);

    let hit = &engine.search("photosynthesis", 1).hits[0];
    assert!(hit.snippet.iter().any(|(s, hl)| *hl && s == "photosynthesis"), "{:?}", hit.snippet);

    // Edits, removals and renames are picked up.
    std::fs::write(docs.join("notes.md"), "Groceries: apples").unwrap();
    std::fs::remove_file(docs.join("sample.pdf")).unwrap();
    std::fs::rename(docs.join("sample.rtf"), docs.join("renamed.rtf")).unwrap();
    std::fs::write(docs.join("fresh.txt"), "a brand new photosynthesis note").unwrap();
    wait_idle(&rx);
    assert_eq!(names(&engine, "marmalade").len(), 5);
    assert_eq!(
        names(&engine, "photosynthesis"),
        ["fresh.txt", "renamed.rtf", "sample.doc", "sample.docx", "sample.html", "sample.odt"]
    );

    // Moving a folder out of the library drops everything inside it.
    let outside = tempfile::tempdir().unwrap();
    std::fs::rename(&docs, outside.path().join("moved")).unwrap();
    wait_idle(&rx);
    assert_eq!(engine.num_docs(), 0);

    // Removing the folder from the library clears it; reopening keeps the index.
    std::fs::rename(outside.path().join("moved"), &docs).unwrap();
    wait_idle(&rx);
    assert_eq!(engine.num_docs(), 9);
    service.set_folders(vec![]);
    assert_eq!(wait_idle(&rx), 0);
}
