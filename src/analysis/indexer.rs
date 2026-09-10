//! The bulkhead around rubydex's indexer: a file it cannot handle costs its own answers.
//!
//! The pinned rev fixes the panic that prompted this — an unwrapped lexical scope on
//! `extend self` inside a `Class.new`/`Module.new` block, reachable from a real bundle — but
//! `create_declaration`'s two unwraps are still upstream, so a version fixes an *instance* of the
//! hazard and not the hazard. Unbulkheaded it is fatal twice over: on the walk the panicking
//! worker is joined with `expect("Worker thread panicked")` and takes the analysis thread with
//! it, and on the buffer route [`rubydex::indexing::index_source`] runs inline, so a few lines
//! typed into an open file kill a server that was already answering.
//!
//! # Why this is not a `catch_unwind` around [`rubydex::indexing::index_files`]
//!
//! Because the failure unit would be the *batch*. rubydex's pool steals work in batches into
//! per-worker queues, so a worker that dies abandons whatever it had already taken, and its peers
//! recover the rest only while they are alive themselves. Measured over a large batch with bad
//! files spread through the order, losses stay at zero until bad files outnumber workers and then
//! climb steeply — deterministically, with no error, no diagnostic and no log line anywhere. A
//! wrapper therefore holds for the rare bug that prompted it and fails for the class of bug a
//! bulkhead exists to bound: an indexer defect on a construct every Rails model writes would kill
//! every worker in the first milliseconds and drop an unbounded fraction of the workspace
//! silently.
//!
//! So the pool is ours, the failure unit is the **file**, and the loop below is rubydex's own
//! shape — workers building local graphs, one serial merge on the calling thread — with the
//! `catch_unwind` moved inside. It costs no measurable throughput against `index_files`.
//!
//! # What is contained, and what deliberately is not
//!
//! Only [`rubydex::indexing::build_local_graph`], which is where the known panics are. The merge
//! that follows is not, and that is a decision rather than an oversight: a panic part-way through
//! `consume_document_changes` leaves the graph in exactly the unknown state
//! [`super::Analysis::resolve`]'s `rebuild()` exists for, and catching it here would hide it
//! behind a graph nobody can trust. Containing the build is what makes the recovery *known* — the
//! graph is itself minus one document, and the document keeps whatever version it had.

use std::{
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    thread,
};

use crossbeam_channel::unbounded;
use rubydex::{
    errors::Errors,
    indexing::{self, IndexerBackend, LanguageId, local_graph::LocalGraph},
    model::graph::Graph,
};

use crate::workspace::DocUri;

/// Which indexer a path's extension asks for.
///
/// 0.2.5 spelled this `LanguageId::from_path`; upstream replaced it with `From<&OsStr>` over
/// the *extension*, so the `map_or` that turns a path with no extension into Ruby is now the
/// caller's. One function rather than two spellings, because `index_buffer` asks the same
/// question of a `DocUri`'s path that the walk asks of a `PathBuf`.
#[must_use]
pub fn language_of(path: &Path) -> LanguageId {
    path.extension().map_or(LanguageId::Ruby, LanguageId::from)
}

/// What indexing a batch of files left behind.
pub struct Batch {
    /// The files rubydex itself would have reported: unreadable, or with no URI to file under.
    pub errors: Vec<Errors>,
    /// The documents whose indexing panicked — not in the graph, and *named* rather than
    /// silently absent, which is the whole difference between this and a wrapper.
    pub skipped: Vec<DocUri>,
}

/// What one file's worth of work produced.
enum Built {
    /// Boxed: a `LocalGraph` is an order of magnitude larger than the other two arms, and this
    /// travels down a channel once per file.
    Indexed(Box<LocalGraph>),
    Failed(Errors),
    Panicked(DocUri),
}

/// Index `paths` into `graph`, losing at most the files that crash the indexer.
///
/// The replacement for [`rubydex::indexing::index_files`]. Paths must be **absolute**:
/// `Url::from_file_path` fails on a relative one, and a document with no URI is indexed
/// nowhere at all — see `core-invariants.md`. A relative path is reported here rather than
/// dropped, which is one thing this loop does that rubydex's also does and the prototype of it
/// did not.
pub fn index_files(graph: &mut Graph, paths: Vec<PathBuf>) -> Batch {
    let mut batch = Batch {
        errors: Vec::new(),
        skipped: Vec::new(),
    };
    if paths.is_empty() {
        return batch;
    }

    // rubydex's own worker count, so the timings above are comparable — bounded by the batch,
    // because a one-file refresh should not start eleven threads to index it.
    let workers = thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .min(paths.len());

    // One file at a time out of a shared queue, and that is the point rather than a detail: a
    // worker holds exactly the file it is on, so there is nothing for it to abandon. The
    // `catch_unwind` below means it never dies anyway; this is what keeps that true if it ever
    // does.
    let (jobs_tx, jobs_rx) = unbounded::<PathBuf>();
    for path in paths {
        // The receiver is this function's own and outlives every send.
        let _ = jobs_tx.send(path);
    }
    drop(jobs_tx);

    let (built_tx, built_rx) = unbounded::<Built>();

    thread::scope(|scope| {
        for _ in 0..workers {
            let jobs = jobs_rx.clone();
            let built = built_tx.clone();
            scope.spawn(move || {
                for path in jobs {
                    // Same: the merge loop below runs until every worker is done.
                    let _ = built.send(build(&path));
                }
            });
        }
        // Or the merge below waits forever for a sender this thread is still holding.
        drop(built_tx);

        // Merged as they arrive, overlapping with the workers, exactly as rubydex does it.
        for outcome in built_rx {
            match outcome {
                Built::Indexed(local) => graph.consume_document_changes(*local),
                Built::Failed(error) => batch.errors.push(error),
                Built::Panicked(uri) => batch.skipped.push(uri),
            }
        }
    });

    batch
}

/// Read one file and index it, off the calling thread.
fn build(path: &Path) -> Built {
    let Ok(source) = std::fs::read_to_string(path) else {
        return Built::Failed(Errors::FileError(format!(
            "Failed to read file `{}`",
            path.display()
        )));
    };
    // `DocUri` rather than `Url::from_file_path` directly, though they are the same call: a
    // document key that is spelled anywhere but here is a document forked in two.
    let Some(uri) = DocUri::from_path(path) else {
        return Built::Failed(Errors::FileError(format!(
            "Couldn't build URI from path `{}`",
            path.display()
        )));
    };
    let language = language_of(path);
    match panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        crash_if_asked(&source);
        indexing::build_local_graph(
            uri.as_str().into(),
            &source,
            &language,
            IndexerBackend::RubyIndexer,
        )
    })) {
        Ok(local) => Built::Indexed(Box::new(local)),
        // The default panic hook has already put rubydex's own file and line on stderr, which
        // is what a bug report is made of. Silencing it would make a contained panic something
        // nobody can report — the same reasoning `Analysis::resolve` gives.
        Err(_) => Built::Panicked(uri),
    }
}

// The counter beside [`CRASHES`], for the one route whose text a test does not write.
//
// Six of the seven routes hand the indexer a file or a buffer, so a test can put the sentinel
// in it. The seventh is `Synthesized::record`, which hands rubydex *RBS this crate generated*
// — nothing a fixture spells reaches that text — so it is armed rather than written. It runs
// inline on the calling thread, which is what makes a `thread_local` the right shape: tests
// running in parallel cannot arm each other's crashes.
#[cfg(test)]
thread_local! {
    /// How many of the next inline indexes a test has asked to crash.
    pub(super) static SOURCE_INDEXES_TO_CRASH: std::cell::Cell<u32> = const {
        std::cell::Cell::new(0)
    };
}

/// Panic where a test asked for one: by the sentinel in the text, or by the armed counter.
#[cfg(test)]
#[cfg_attr(coverage_nightly, coverage(off))]
fn crash_if_asked(source: &str) {
    assert!(
        !source.contains(CRASHES),
        "a stand-in for rubydex ruby_indexer.rs:985, asked for by the text"
    );
    let remaining = SOURCE_INDEXES_TO_CRASH.get();
    if remaining > 0 {
        SOURCE_INDEXES_TO_CRASH.set(remaining - 1);
        panic!("a stand-in for rubydex ruby_indexer.rs:985, on the route no fixture reaches");
    }
}

/// Index one in-memory document, and say whether it survived.
///
/// The replacement for [`rubydex::indexing::index_source`], which is the same two lines with
/// the first of them contained. Five call sites run this **inline on the analysis thread** —
/// the two `.rbs`/template pre-passes, both arms of `index_buffer`, and the generated RBS a
/// synthesizer records — so this is the half of the bug a user meets while working rather than
/// while waiting.
///
/// A `false` costs the document its *update* and nothing else: the panic is in the build, so
/// the graph is never entered and whatever version it already held still answers.
pub fn index_source(graph: &mut Graph, uri: &str, source: &str, language: &LanguageId) -> bool {
    let built = panic::catch_unwind(AssertUnwindSafe(|| {
        #[cfg(test)]
        crash_if_asked(source);
        indexing::build_local_graph(uri.into(), source, language, IndexerBackend::RubyIndexer)
    }));
    match built {
        Ok(local) => {
            graph.consume_document_changes(local);
            true
        }
        Err(_) => false,
    }
}

/// A file the indexer cannot handle, for the tests that need one — and it is a stand-in.
///
/// **No real input provokes a panic at the pinned rev.** A scan of 160,341 files — every Ruby
/// and RBS file under five installed Rubies, six Rails corpora, four bench gem homes and
/// `vendor/rbs` — finds none, so there is nothing to write here and the exception
/// `concurrency.md` grants `RESOLVES_TO_CRASH` and `Task::Panic` covers this module too.
///
/// **That is not an argument against the bulkhead.** `create_declaration`'s two unwraps are
/// still upstream, so the class of bug outlives any version that retires one instance of it.
///
/// A Ruby comment, so a fixture that holds it is still a file that would otherwise index; and
/// recognised in the *text* rather than through a counter, because [`index_files`] builds on
/// worker threads and a `thread_local` cannot reach one.
#[cfg(test)]
pub(super) const CRASHES: &str = "# ya-lsp test: crash the indexer\n";

#[cfg(test)]
mod tests {
    use super::*;
    use rubydex::model::ids::UriId;

    fn holds(graph: &Graph, path: &Path) -> bool {
        let uri = DocUri::from_path(path).expect("an absolute path");
        graph.documents().contains_key(&UriId::from(uri.as_str()))
    }

    fn write(root: &Path, name: &str, source: &str) -> PathBuf {
        let path = root.join(name);
        std::fs::write(&path, source).expect("write");
        path
    }

    #[test]
    fn a_file_that_crashes_the_indexer_costs_that_file_and_no_other() {
        let root = tempfile::tempdir().expect("tempdir");
        let good = write(root.path(), "good.rb", "class Good\n  def hi = 1\nend\n");
        let bad = write(root.path(), "bad.rb", CRASHES);
        let also = write(root.path(), "also.rb", "class Also; end\n");

        let mut graph = Graph::new();
        let batch = index_files(&mut graph, vec![good.clone(), bad.clone(), also.clone()]);

        assert!(holds(&graph, &good));
        assert!(holds(&graph, &also));
        assert!(!holds(&graph, &bad));
        assert!(batch.errors.is_empty(), "{:?}", batch.errors);
        assert_eq!(
            batch.skipped,
            vec![DocUri::from_path(&bad).expect("an absolute path")]
        );
    }

    /// The measurement that is the whole reason this module exists rather than a `catch_unwind`
    /// around `rubydex::indexing::index_files`.
    ///
    /// More bad files than the machine has workers is where a wrapper starts losing good ones —
    /// 11 bad files cost 4 good ones on the 11-worker machine that measured it, 40 cost 181 —
    /// silently, with no error and no log line. The bad files are spread through the order
    /// rather than clustered, because that is what the sweep did and what a real bundle looks
    /// like.
    #[test]
    fn more_bad_files_than_workers_loses_exactly_those_files() {
        let root = tempfile::tempdir().expect("tempdir");
        let workers = thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        let bad_count = workers * 4;

        let mut paths = Vec::new();
        let mut bad = Vec::new();
        for index in 0..bad_count * 8 {
            let (name, source) = if index % 8 == 3 {
                (format!("bad{index}.rb"), CRASHES.to_owned())
            } else {
                (
                    format!("good{index}.rb"),
                    format!("class Good{index}; end\n"),
                )
            };
            let path = write(root.path(), &name, &source);
            if index % 8 == 3 {
                bad.push(path.clone());
            }
            paths.push(path);
        }

        let mut graph = Graph::new();
        let batch = index_files(&mut graph, paths.clone());

        assert_eq!(bad.len(), bad_count);
        let lost: Vec<&PathBuf> = paths
            .iter()
            .filter(|path| !holds(&graph, path) && !bad.contains(path))
            .collect();
        assert!(lost.is_empty(), "good files lost: {lost:?}");
        assert_eq!(batch.skipped.len(), bad_count);
        for path in &bad {
            assert!(!holds(&graph, path));
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_reported_the_way_rubydex_reports_it() {
        let root = tempfile::tempdir().expect("tempdir");
        // A directory reads as an error on every platform this ships on, and it needs no
        // permission bit that a CI runner might already hold.
        let directory = root.path().join("not_a_file");
        std::fs::create_dir(&directory).expect("mkdir");

        let mut graph = Graph::new();
        let batch = index_files(&mut graph, vec![directory.clone()]);

        assert!(batch.skipped.is_empty());
        assert_eq!(
            format!("{:?}", batch.errors),
            format!(
                "[FileError(\"Failed to read file `{}`\")]",
                directory.display()
            )
        );
    }

    /// `core-invariants.md`: `Url::from_file_path` fails on a relative path, and rubydex's own
    /// loop reports that rather than indexing nothing. The prototype of this one did not, and
    /// spent a round measuring two no-ops against each other.
    #[test]
    fn a_relative_path_is_reported_rather_than_silently_indexing_nothing() {
        // A path that really reads and really has no URI, which is the only way to reach the
        // second arm. cargo runs a test binary with the package root as its working directory,
        // so this is the one relative path that is certain to exist without the test writing
        // one — and writing one would mean changing the process's directory, which the rest of
        // the suite is running in.
        let mut graph = Graph::new();
        let batch = index_files(&mut graph, vec![PathBuf::from("Cargo.toml")]);

        assert!(batch.skipped.is_empty());
        assert_eq!(
            format!("{:?}", batch.errors),
            "[FileError(\"Couldn't build URI from path `Cargo.toml`\")]"
        );
    }

    #[test]
    fn an_empty_batch_starts_no_threads_and_reports_nothing() {
        let mut graph = Graph::new();
        // Not `is_empty`: a fresh graph already holds rubydex's own synthetic `built-in`
        // document, which is why `DocUri::from_uri_str` rejects that URI in the first place.
        let before = graph.documents().len();
        let batch = index_files(&mut graph, Vec::new());
        assert!(batch.errors.is_empty());
        assert!(batch.skipped.is_empty());
        assert_eq!(graph.documents().len(), before);
    }

    /// The inline route, and the property that makes the recovery a *known* state: the panic
    /// is in the build, so the graph is never entered and the document keeps the version it had.
    #[test]
    fn a_document_that_crashes_the_indexer_keeps_the_version_it_had() {
        let mut graph = Graph::new();
        let uri = "file:///buffer.rb";

        assert!(index_source(
            &mut graph,
            uri,
            "class Buffer\n  def hi = 1\nend\n",
            &LanguageId::Ruby
        ));
        let before = graph.documents().len();

        assert!(!index_source(&mut graph, uri, CRASHES, &LanguageId::Ruby));
        assert_eq!(graph.documents().len(), before);
        assert!(graph.documents().contains_key(&UriId::from(uri)));
    }
}
