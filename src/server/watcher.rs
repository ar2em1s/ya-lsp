//! A filesystem watcher of the server's own, for a client that cannot register one.
//!
//! **Everything after the event is shared with the client's watcher.** `Task::WatchedFiles`,
//! `Analysis::refresh`, the `Workspace::indexes` predicate and the debounce all serve
//! `workspace/didChangeWatchedFiles`. This module only adds the event source for closed files. It
//! produces exactly the task the client's watcher produces, so nothing downstream knows which one
//! it came from, with one deliberate exception: `Watched`, which decides whether a saved buffer
//! yields to the disk.
//!
//! **It runs only when the client cannot.** `capabilities::watched_files` returns `None` for a
//! client that does not offer `workspace.didChangeWatchedFiles.dynamicRegistration`, and that is
//! the whole condition. A client that took the registration keeps sending events, and the server
//! does not watch the same tree twice. VS Code takes it; Claude Code, Helix, eglot and Neovim on
//! Linux do not.
//!
//! **What it does not fix:**
//! - a filesystem watcher sees saved files, never another editor's unsaved buffer;
//! - two clients on one project still mean two servers and two graphs.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use notify::{RecursiveMode, Watcher as _};

use crate::analysis::{Task, Watched};
use crate::workspace::{DocUri, config::IndexConfig, watched_directories};

/// How long one batch of filesystem events is gathered before it is sent on.
///
/// A window, not a quiet period: a `git checkout` writes continuously for longer than any quiet
/// period, so waiting for the writing to stop would hold the whole checkout in this thread. The
/// batch is sent every `DEBOUNCE` while events keep arriving, and `Analysis::refresh` deduplicates
/// what it is handed anyway.
///
/// Well under the resolve debounce on purpose. The point of gathering is that one
/// `Task::WatchedFiles` carrying a thousand paths costs one settle, and a thousand tasks carrying
/// one path each cost a thousand.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// A running watcher. Dropping it stops the thread and releases every OS handle.
///
/// **Drop it before `AnalysisHandle::join`.** The collector thread holds a clone of the analysis
/// `Sender`: a live sender means the channel never disconnects, the run loop never breaks, and the
/// join waits forever. `concurrency.md` records the same hazard for the test harness.
pub struct Watching {
    /// Dropping this asks the collector thread to stop; it is the only thing sent on it.
    stop: Option<Sender<()>>,
    collector: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watching {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(collector) = self.collector.take() {
            // A collector that panicked was already reported by the default hook; nothing can be
            // done about it during shutdown.
            let _ = collector.join();
        }
    }
}

/// Start watching `root`, or explain in the log why nothing is watched.
///
/// `config` is the project's `ya-lsp.toml` as a URI, spelled the way an incoming path will be. A
/// change to it is [`Task::ReloadConfig`], far more work than re-indexing one file. The split is
/// made here for the reason `route_notification` makes it: routing belongs to whoever received the
/// event.
///
/// **Nothing here touches the filesystem; the thread does.** Arming walks the whole project (the
/// walk the index just ran, paid again) and, on Linux, calls `inotify_add_watch` per directory. No
/// client waits on that, and it can surprise: a network mount, a repository with forty thousand
/// directories. So it runs on the collector thread, this function returns as soon as that thread
/// exists, and the first request is answered while the watch is still coming up.
///
/// The cost of that window: a change made before the watch is armed is missed, a restart's worth of
/// staleness at worst. The log line says how long arming took, the one number a report about a
/// missed change needs.
pub fn watch(
    root: PathBuf,
    index: IndexConfig,
    config: Option<DocUri>,
    tasks: Sender<Task>,
) -> Option<Watching> {
    let (stop, stopped) = crossbeam_channel::bounded(0);
    let collector = std::thread::Builder::new()
        .name("ya-lsp-watcher".to_owned())
        .spawn(move || {
            // The one place a watcher that could not start is said out loud. `arm` reports rather
            // than logs so a test can ask it directly: the sink `captured_logs` installs is
            // thread-local, and this is not that thread.
            let mut state = match Collector::arm(root, index, config, tasks) {
                Ok(state) => state,
                Err(error) => {
                    tracing::warn!(
                        "{}",
                        crate::messages::cannot_watch_files(&error.to_string())
                    );
                    return;
                }
            };
            state.run(&stopped);
        });
    match collector {
        Ok(collector) => Some(Watching {
            stop: Some(stop),
            collector: Some(collector),
        }),
        Err(error) => {
            tracing::warn!(
                "{}",
                crate::messages::cannot_watch_files(&error.to_string())
            );
            None
        }
    }
}

/// One recursive watch, or one per directory, depending on the platform.
///
/// **Linux: one watch per directory.** inotify charges that either way: a recursive watch is one
/// per directory underneath, ignored ones included. A large repository with a vendored bundle can
/// exhaust `max_user_watches`, and that fails as a watcher that silently stops seeing part of the
/// tree. Registering from the walk's own list keeps `node_modules` and `vendor/bundle` out of the
/// count, as rust-analyzer's `vfs-notify` does, for the same reason.
///
/// **macOS: one FSEvents stream** for the whole root, with no such limit. **Windows: one
/// `ReadDirectoryChangesW` handle.** There the directory list is a filter, not a registration (see
/// [`Collector::interesting`]). The platforms disagree about *where* the list is applied, never
/// about what is on it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn arm(
    watcher: &mut notify::RecommendedWatcher,
    root: &Path,
    _directories: &HashSet<PathBuf>,
) -> notify::Result<()> {
    watcher.watch(root, RecursiveMode::Recursive)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn arm(
    watcher: &mut notify::RecommendedWatcher,
    _root: &Path,
    directories: &HashSet<PathBuf>,
) -> notify::Result<()> {
    // The root is in `directories` (the walk's first entry), so it is not armed separately. One
    // failure is not all of them: a directory deleted between the walk and here is ordinary, and
    // watching nothing because of it would be the worst answer.
    let mut armed = 0_usize;
    for directory in directories {
        match watcher.watch(directory, RecursiveMode::NonRecursive) {
            Ok(()) => armed += 1,
            Err(error) => tracing::debug!("not watching {}: {error}", directory.display()),
        }
    }
    if armed == 0 {
        return Err(notify::Error::generic("no directory could be watched"));
    }
    Ok(())
}

struct Collector {
    /// Held here so it lives exactly as long as the thread reading its events.
    ///
    /// Read only on the platforms that arm one watch per directory. Elsewhere the single recursive
    /// watch is made once and never touched again; this field keeps it alive.
    #[cfg_attr(
        any(target_os = "macos", target_os = "windows"),
        expect(dead_code, reason = "one recursive watch, held rather than re-armed")
    )]
    watcher: notify::RecommendedWatcher,
    root: PathBuf,
    /// The `index` as it was at startup. It does not follow a reload.
    ///
    /// **A known limit.** A `ya-lsp.toml` that widens `index.exclude` after startup does not change
    /// what this thread listens to: the reloaded configuration lives on the analysis thread, which
    /// this one cannot see. It costs notifications that `Analysis::refresh` drops, never a wrong
    /// answer. Whether a change is *indexed* is decided by `Workspace::indexes` on the live
    /// configuration, there, not here.
    index: IndexConfig,
    config: Option<DocUri>,
    /// The root as the filesystem resolves it, when that differs from the client's spelling.
    ///
    /// **macOS reaches every temp directory, `/tmp` and `/var` through a symlink, and FSEvents
    /// reports the resolved path.** So an event arrives spelled differently from every document the
    /// server holds, `Workspace::indexes` strips the *workspace's* prefix, and the answer is no to
    /// everything. Silently, because a watcher that reports nothing looks exactly like a tree where
    /// nothing changed.
    ///
    /// `workspace::resolve_load_path` meets the same collision for a configured load path and
    /// settles it the same way: canonical for reaching disk, the root's own spelling for anything
    /// inside it.
    ///
    /// `None` when the two agree: the ordinary case, and every case on Linux.
    resolved: Option<PathBuf>,
    directories: HashSet<PathBuf>,
    /// Directories the walk already declined, so one `.git` costs one walk, not one per event.
    ///
    /// Without it, every write under an ignored directory looks new to [`Collector::take_in`] (it
    /// is not in `directories` and never will be), so a rebase would re-walk the whole project once
    /// per object git writes.
    ///
    /// Never pruned: a directory the walk declined is declined for the same reason next time, and
    /// the set is bounded by the ignored directories the project actually touches.
    refused: HashSet<PathBuf>,
    tasks: Sender<Task>,
    incoming: Receiver<notify::Result<notify::Event>>,
}

impl Collector {
    /// Everything that can fail, on the thread that will do the watching.
    fn arm(
        root: PathBuf,
        index: IndexConfig,
        config: Option<DocUri>,
        tasks: Sender<Task>,
    ) -> notify::Result<Self> {
        let started = Instant::now();
        let (events, incoming) = crossbeam_channel::unbounded();
        let mut watcher = notify::recommended_watcher(move |event| {
            // The backend's own thread, so nothing here may block or fail loudly. A send that
            // cannot land means the collector is gone, which is shutdown.
            let _ = events.send(event);
        })?;
        let directories: HashSet<PathBuf> =
            watched_directories(&root, &index).into_iter().collect();
        arm(&mut watcher, &root, &directories)?;
        // The duration is in the line because it varies by two orders of magnitude between
        // machines, and a report about a missed change needs it.
        tracing::info!(
            "watching {} for the changes this editor cannot report, {} directories, armed in \
             {:.2?}",
            root.display(),
            directories.len(),
            started.elapsed()
        );
        let resolved = root
            .canonicalize()
            .ok()
            .filter(|canonical| *canonical != root);
        Ok(Self {
            watcher,
            root,
            index,
            config,
            resolved,
            directories,
            refused: HashSet::new(),
            tasks,
            incoming,
        })
    }

    /// The same collector with nothing armed, for the half of this module that is bookkeeping.
    ///
    /// Arming is what costs (the walk in [`watch`]'s docs), and none of the batching, the spelling
    /// or the `ya-lsp.toml` split needs it. Building a `notify` watcher without calling `watch`
    /// takes microseconds, so these tests run as fast as any other, and the tests that really watch
    /// stay as few as they can be.
    #[cfg(test)]
    fn unarmed(root: PathBuf, directories: HashSet<PathBuf>, tasks: Sender<Task>) -> Self {
        let (_events, incoming) = crossbeam_channel::unbounded();
        Self {
            watcher: notify::recommended_watcher(|_| {}).expect("a watcher"),
            config: DocUri::from_path(&root.join("ya-lsp.toml")),
            resolved: root
                .canonicalize()
                .ok()
                .filter(|canonical| *canonical != root),
            root,
            index: IndexConfig::default(),
            directories,
            refused: HashSet::new(),
            tasks,
            incoming,
        }
    }

    /// An event's path, spelled the way the workspace spells what is inside its root.
    ///
    /// See [`Collector::resolved`]. A path outside the root is left as it arrived: nothing here
    /// knows a better spelling, and `interesting` will drop it.
    fn spell(&self, path: PathBuf) -> PathBuf {
        let Some(resolved) = self.resolved.as_ref() else {
            return path;
        };
        match path.strip_prefix(resolved) {
            Ok(relative) => self.root.join(relative),
            Err(_) => path,
        }
    }

    fn run(&mut self, stopped: &Receiver<()>) {
        loop {
            let first = crossbeam_channel::select! {
                recv(stopped) -> _ => return,
                recv(self.incoming) -> event => match event {
                    Ok(event) => event,
                    // The watcher went away, which only happens on shutdown.
                    Err(_) => return,
                },
            };

            let mut paths = Vec::new();
            self.gather(first, &mut paths);
            // An absolute deadline, not a quiet period: see `DEBOUNCE`.
            let deadline = Instant::now() + DEBOUNCE;
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                // Cloned out of `self` so the borrow does not fight `gather`, which needs
                // `&mut self` to take in a directory that just appeared.
                let next = self.incoming.recv_timeout(left);
                match next {
                    Ok(event) => self.gather(event, &mut paths),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => break,
                }
            }

            if !self.send(paths) {
                // The analysis thread has stopped, so there is nobody left to tell.
                return;
            }
        }
    }

    /// One event's paths, with a directory that has just appeared taken in as well.
    fn gather(&mut self, event: notify::Result<notify::Event>, paths: &mut Vec<PathBuf>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                // Debug, not warn: a watch failing on one directory during a checkout is ordinary,
                // and `notify` reports a dropped overflow the same way.
                tracing::debug!("watching: {error}");
                return;
            }
        };
        // Reading a file is not changing it, and every backend reports both.
        if matches!(event.kind, notify::EventKind::Access(_)) {
            return;
        }
        // `is_dir` is a syscall and a checkout is thousands of events, so it is asked only of the
        // two kinds that can introduce a directory, not of every path.
        let may_be_new = matches!(
            event.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
        for path in event.paths {
            let path = self.spell(path);
            if may_be_new
                && !self.directories.contains(&path)
                && !self.refused.contains(&path)
                && path.is_dir()
            {
                self.take_in(&path, paths);
            }
            paths.push(path);
        }
    }

    /// A directory that was not there when the walk ran.
    ///
    /// The walk is run again, not reasoned about: a `git checkout` can create a whole tree at once,
    /// and only the walk knows which directories are ignored. On Linux each new one is armed;
    /// elsewhere the recursive watch already covers it, and the list decides which events are kept.
    ///
    /// **Its files are added to the batch.** On Linux their events arrived before anything was
    /// listening; on macOS, before the directory was on the list. `Analysis::refresh` decides which
    /// of them it indexes.
    fn take_in(&mut self, appeared: &Path, paths: &mut Vec<PathBuf>) {
        for found in watched_directories(&self.root, &self.index) {
            if !self.directories.insert(found.clone()) {
                continue;
            }
            #[cfg(not(any(target_os = "macos", target_os = "windows")))]
            if let Err(error) = self.watcher.watch(&found, RecursiveMode::NonRecursive) {
                tracing::debug!("not watching {}: {error}", found.display());
            }
            let Ok(entries) = std::fs::read_dir(&found) else {
                continue;
            };
            paths.extend(
                entries
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| path.is_file()),
            );
        }
        if !self.directories.contains(appeared) {
            // The walk has seen it and does not want it: `.git`, `node_modules`, a vendored bundle.
            // Remembered so the next write inside it is not another walk.
            self.refused.insert(appeared.to_path_buf());
        }
    }

    /// Whether a path is one this watcher listens for at all.
    ///
    /// The directory list, applied to the *parent*: a file is interesting when the walk went into
    /// the directory holding it.
    /// - macOS and Windows: this is the whole filter. One recursive watch reports `.git` rewriting
    ///   itself during a rebase and every write into `tmp` and `log`.
    /// - Linux: those directories were never armed, so this says yes to everything that arrives,
    ///   for one hash lookup.
    fn interesting(&self, path: &Path) -> bool {
        path.parent()
            .is_some_and(|parent| self.directories.contains(parent))
    }

    /// Turn one batch into one task. Returns false once the analysis thread has stopped.
    fn send(&mut self, paths: Vec<PathBuf>) -> bool {
        let mut seen = HashSet::with_capacity(paths.len());
        let mut uris = Vec::with_capacity(paths.len());
        let mut gone = Vec::new();
        let mut reload = false;
        for path in paths {
            if !self.interesting(&path) {
                continue;
            }
            // A watched directory that just stopped being one. Collected, not pruned here, for the
            // reason below.
            if self.directories.contains(&path) && !path.is_dir() {
                gone.push(path.clone());
            }
            let Some(uri) = DocUri::from_path(&path) else {
                continue;
            };
            if self.config.as_ref() == Some(&uri) {
                // As `route_notification` does it: the reload re-indexes the whole workspace from
                // disk, which covers everything else in this batch and more.
                reload = true;
                break;
            }
            if seen.insert(uri.clone()) {
                uris.push(uri);
            }
        }
        // Pruned only now, deliberately. A deleted file's own directory has just stopped existing,
        // and pruning before the filter above would throw its deletion away: the one change nothing
        // else can tell the index about. Driven off the batch, not by re-statting the whole set,
        // which on a large repository is a thousand syscalls every hundred milliseconds of a
        // checkout.
        for directory in gone {
            self.directories.remove(&directory);
        }

        if reload {
            return self.tasks.send(Task::ReloadConfig).is_ok();
        }
        if uris.is_empty() {
            return true;
        }
        tracing::debug!(paths = uris.len(), "the server's own watcher saw changes");
        self.tasks
            .send(Task::WatchedFiles {
                uris,
                watched: Watched::ByTheServer,
            })
            .is_ok()
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// How long a test waits for the operating system to report a write.
    ///
    /// **A timeout, never a sleep.** Every wait returns as soon as the task arrives, so the
    /// constant only bounds a failure; when nothing is wrong, the whole module runs in a couple of
    /// seconds.
    ///
    /// On macOS, `FSEventStreamStart` blocks in a Mach RPC to `fseventsd` whose duration scales
    /// with the number of entries in the directory the *calling executable* sits in, not the
    /// watched one. A test binary lives in `target/debug/deps`, where macOS' default
    /// `split-debuginfo = "unpacked"` leaves every codegen object of every build.
    /// `[profile.dev] split-debuginfo = "packed"` in `Cargo.toml` keeps that directory small and
    /// arming fast.
    ///
    /// A minute, not a second, because it bounds a *failure*: a loaded CI runner is slow, and a
    /// checkout whose `target/` predates that profile line still pays the old cost. It should fail
    /// on an assertion, not on the clock.
    const PATIENCE: Duration = Duration::from_secs(60);

    struct Fixture {
        root: tempfile::TempDir,
        _watching: Watching,
        tasks: Receiver<Task>,
    }

    impl Fixture {
        /// The root as the *client* spells it, symlinks and all.
        ///
        /// Deliberately not canonicalized. On macOS a `TempDir` is under `/var/folders`, a symlink
        /// to `/private/var/folders`, so every test here also tests [`Collector::spell`]; without
        /// it they all fail, reporting nothing.
        fn root(&self) -> PathBuf {
            self.root.path().to_path_buf()
        }

        /// Block until the operating system is really listening.
        ///
        /// [`watch`] deliberately returns before the watch is armed (see its docs for why). A test
        /// that writes a file straight afterwards can write into the window where nothing is
        /// watching, then wait out `PATIENCE` for an event that never comes.
        ///
        /// There is no handshake to wait on, and production has no use for one, so this asks the
        /// way a person would: write something and see whether it comes back. The probe is at the
        /// root, which is watched on every platform and under every configuration.
        fn armed(&self) {
            let probe = self.root().join("probe.rb");
            let deadline = Instant::now() + PATIENCE;
            while deadline > Instant::now() {
                std::fs::write(&probe, "class Probe\nend\n").expect("the probe file");
                if self.tasks.recv_timeout(Duration::from_millis(250)).is_ok() {
                    std::fs::remove_file(&probe).expect("the probe file");
                    return;
                }
            }
            panic!("the watcher never armed");
        }

        /// Every path reported until one ends in `wanted`, or until `PATIENCE` runs out.
        ///
        /// Returns what it saw either way, so a test can assert both what arrived and what did not:
        /// the only way to say "and nothing under `.git`" without a sleep.
        fn until(&self, wanted: &str) -> (bool, Vec<String>) {
            let mut seen = Vec::new();
            let deadline = Instant::now() + PATIENCE;
            while let Some(left) = deadline.checked_duration_since(Instant::now()) {
                match self.tasks.recv_timeout(left) {
                    Ok(Task::WatchedFiles { uris, watched }) => {
                        assert_eq!(watched, Watched::ByTheServer);
                        seen.extend(uris.iter().map(|uri| uri.as_str().to_owned()));
                        if seen.iter().any(|uri| uri.ends_with(wanted)) {
                            return (true, seen);
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
            (false, seen)
        }
    }

    fn watching(files: &[(&str, &str)]) -> Fixture {
        // Each fixture has a tempdir of its own and shares nothing but the operating system, so
        // these run in parallel.
        let root = tempfile::tempdir().expect("tempdir");
        for (relative, contents) in files {
            let path = root.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("the directory");
            std::fs::write(path, contents).expect("the file");
        }
        let (tasks, incoming) = crossbeam_channel::unbounded();
        let path = root.path().to_path_buf();
        let watching = watch(
            path.clone(),
            IndexConfig::default(),
            Some(DocUri::from_path(&path.join("ya-lsp.toml")).expect("a uri")),
            tasks,
        )
        .expect("a watcher");
        let fixture = Fixture {
            root,
            _watching: watching,
            tasks: incoming,
        };
        fixture.armed();
        fixture
    }

    /// A collector over `root` watching `root` and `root/app`, with nothing armed.
    fn bookkeeping(root: &Path) -> (Collector, Receiver<Task>) {
        let (tasks, incoming) = crossbeam_channel::unbounded();
        let directories = [root.to_path_buf(), root.join("app")].into_iter().collect();
        (
            Collector::unarmed(root.to_path_buf(), directories, tasks),
            incoming,
        )
    }

    #[test]
    fn one_batch_is_one_task_with_each_path_once() {
        // The reason for gathering at all: a `git checkout` names the same file twice (a create and
        // a change, or a rewrite and a mode change), and one task with a thousand paths costs one
        // settle where a thousand tasks cost a thousand.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, tasks) = bookkeeping(root.path());

        assert!(collector.send(vec![
            root.path().join("app/story.rb"),
            root.path().join("app/story.rb"),
            root.path().join("app/comment.rb"),
        ]));

        match tasks.try_recv().expect("one task") {
            Task::WatchedFiles { uris, watched } => {
                assert_eq!(uris.len(), 2, "{uris:?}");
                assert_eq!(watched, Watched::ByTheServer);
            }
            other => panic!("{other:?}"),
        }
        assert!(tasks.try_recv().is_err(), "one batch is one task");
    }

    #[test]
    fn a_batch_with_nothing_in_it_worth_reporting_sends_nothing() {
        // The common case on macOS and Windows, where one recursive watch reports every write under
        // `.git` and `tmp`. A task per batch regardless would wake the analysis thread for nothing:
        // the cost this filter exists to avoid.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, tasks) = bookkeeping(root.path());

        assert!(collector.send(vec![
            root.path().join(".git/objects/abcdef"),
            std::path::PathBuf::from("/elsewhere/entirely/thing.rb"),
        ]));

        assert!(tasks.try_recv().is_err());
    }

    #[test]
    fn ya_lsp_toml_wins_the_whole_batch() {
        // A reload re-indexes the workspace from disk, so it covers everything else in the batch
        // and more. That is `route_notification`'s rule, where the client's own watcher meets the
        // same question.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, tasks) = bookkeeping(root.path());

        assert!(collector.send(vec![
            root.path().join("app/story.rb"),
            root.path().join("ya-lsp.toml"),
            root.path().join("app/comment.rb"),
        ]));

        assert!(matches!(tasks.try_recv(), Ok(Task::ReloadConfig)));
        assert!(tasks.try_recv().is_err(), "and nothing else");
    }

    #[test]
    fn a_directory_that_has_gone_is_forgotten_after_its_own_deletions_are_reported() {
        // The ordering `send` spells out: a deleted file's directory has stopped existing, and
        // pruning before the filter would throw the deletion away, the one change nothing else can
        // tell the index about.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, tasks) = bookkeeping(root.path());
        let gone = root.path().join("app");

        assert!(collector.send(vec![gone.join("story.rb"), gone.clone()]));

        match tasks.try_recv().expect("one task") {
            Task::WatchedFiles { uris, .. } => assert!(
                uris.iter().any(|uri| uri.as_str().ends_with("story.rb")),
                "the deletion was dropped with its directory: {uris:?}"
            ),
            other => panic!("{other:?}"),
        }
        assert!(
            !collector.directories.contains(&gone),
            "a directory that is not a directory any more is not watched"
        );
    }

    #[test]
    fn a_root_the_filesystem_spells_the_same_way_is_not_translated() {
        // The other half of `spell`, and the ordinary case on Linux and for most real projects:
        // nothing between the root and disk is a symlink, so the path arrives as the workspace
        // already writes it. It needs its own test because the fixtures above all sit under a macOS
        // temp directory, where this never happens.
        let root = tempfile::tempdir().expect("tempdir");
        let canonical = root.path().canonicalize().expect("a real directory");
        let (tasks, _incoming) = crossbeam_channel::unbounded();
        let collector = Collector::unarmed(canonical.clone(), HashSet::new(), tasks);

        assert!(
            collector.resolved.is_none(),
            "there is nothing to translate"
        );
        let path = canonical.join("app/story.rb");
        assert_eq!(collector.spell(path.clone()), path);
    }

    #[test]
    fn a_directory_already_watched_or_already_declined_is_not_walked_again() {
        // `take_in` re-runs the whole walk, so the two cheap checks in front of it are what keep a
        // rebase from paying one walk per object git writes: a directory already watched is not
        // new, and neither is one the walk already refused.
        let root = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(root.path().join("app")).expect("the directory");
        std::fs::create_dir_all(root.path().join(".git")).expect("the directory");
        let (mut collector, _tasks) = bookkeeping(root.path());
        collector.refused.insert(root.path().join(".git"));

        let mut paths = Vec::new();
        for known in ["app", ".git"] {
            collector.gather(
                Ok(notify::Event::new(notify::EventKind::Create(
                    notify::event::CreateKind::Folder,
                ))
                .add_path(root.path().join(known))),
                &mut paths,
            );
        }

        assert_eq!(paths.len(), 2, "both are still reported: {paths:?}");
        assert_eq!(
            collector.refused.len(),
            1,
            "neither of them ran the walk again"
        );
    }

    #[test]
    fn a_root_that_is_not_there_is_not_watched() {
        // The one failure this module can really be given: a root that vanished between the
        // handshake and the thread starting. Every backend refuses it, and refusing is right; what
        // must not happen is a `Watching` that looks armed and reports nothing. Asked of `arm`, not
        // `watch`, because the warning is written on a thread the test's log sink cannot reach.
        let root = tempfile::tempdir().expect("tempdir");
        let (tasks, _incoming) = crossbeam_channel::unbounded();

        let refused = Collector::arm(
            root.path().join("not-here"),
            IndexConfig::default(),
            None,
            tasks,
        );

        assert!(refused.is_err(), "a root that is not there was watched");
    }

    #[test]
    fn a_path_that_cannot_be_spelled_as_a_uri_is_dropped() {
        // Everything downstream is keyed by `DocUri`, an absolute `file:` URL. A path that cannot
        // become one has nowhere to go, so it is dropped, not unwrapped: these come from the
        // operating system, and a watcher that panics on one loses file watching for the rest of
        // the session.
        let (tasks, incoming) = crossbeam_channel::unbounded();
        let relative = std::path::PathBuf::from("app/models");
        let mut collector = Collector::unarmed(
            std::path::PathBuf::from("."),
            [relative.clone()].into_iter().collect(),
            tasks,
        );

        assert!(collector.send(vec![relative.join("story.rb")]));
        assert!(incoming.try_recv().is_err());
    }

    #[test]
    fn a_collector_with_nobody_listening_stops() {
        // The analysis thread has gone, which on shutdown happens first. There is nobody left to
        // tell, so the run loop returns instead of batching on.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, tasks) = bookkeeping(root.path());
        drop(tasks);

        assert!(!collector.send(vec![root.path().join("app/story.rb")]));
    }

    #[test]
    fn an_error_from_the_backend_is_not_an_event_and_neither_is_a_read() {
        // `notify` reports a dropped overflow as an error, and every backend reports reads as
        // events. Neither is a change, and a batch of only those is not a task.
        let root = tempfile::tempdir().expect("tempdir");
        let (mut collector, _tasks) = bookkeeping(root.path());
        let mut paths = Vec::new();

        collector.gather(Err(notify::Error::generic("overflow")), &mut paths);
        collector.gather(
            Ok(
                notify::Event::new(notify::EventKind::Access(notify::event::AccessKind::Read))
                    .add_path(root.path().join("app/story.rb")),
            ),
            &mut paths,
        );

        assert!(paths.is_empty(), "{paths:?}");
    }

    #[test]
    fn a_file_created_edited_and_deleted_on_disk_is_reported_each_time() {
        // The point of the module: the three events a `git checkout` is made of. No client said
        // anything; only a filesystem watcher can see these writes.
        let watched = watching(&[("app/models/story.rb", "class Story\nend\n")]);
        let path = watched.root().join("app/models/comment.rb");

        std::fs::write(&path, "class Comment\nend\n").expect("write");
        assert!(watched.until("comment.rb").0, "a file created on disk");

        std::fs::write(&path, "class Comment\n  def body\n  end\nend\n").expect("write");
        assert!(watched.until("comment.rb").0, "a file edited on disk");

        std::fs::remove_file(&path).expect("remove");
        assert!(watched.until("comment.rb").0, "a file deleted on disk");
    }

    #[test]
    fn a_directory_that_did_not_exist_at_startup_is_taken_in() {
        // The gap a walk-once watcher has, and why `take_in` exists: on Linux nothing listens
        // inside a directory that was not there to be armed, so a file written into a brand-new
        // `app/services/` would be invisible for the life of the server.
        let watched = watching(&[("app/models/story.rb", "class Story\nend\n")]);
        let root = watched.root();

        std::fs::create_dir_all(root.join("app/services")).expect("the directory");
        std::fs::write(root.join("app/services/publish.rb"), "class Publish\nend\n")
            .expect("write");

        assert!(watched.until("publish.rb").0, "a file in a new directory");
    }

    #[test]
    fn ya_lsp_toml_is_a_reload_and_not_a_file_to_re_index() {
        // The split `route_notification` makes for the client's own watcher: a reload drops the
        // whole graph and re-runs the gem index, so it is reserved for the one file that decides
        // what the whole index is.
        let watched = watching(&[("app/models/story.rb", "class Story\nend\n")]);
        let root = watched.root();

        std::fs::write(root.join("ya-lsp.toml"), "[index]\nmax_files = 10\n").expect("write");

        let deadline = Instant::now() + PATIENCE;
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match watched.tasks.recv_timeout(left) {
                Ok(Task::ReloadConfig) => return,
                Ok(_) => {}
                Err(_) => break,
            }
        }
        panic!("ya-lsp.toml changing on disk did not ask for a reload");
    }

    #[test]
    fn a_directory_the_walk_never_went_into_is_not_reported() {
        // The cost half, invisible when it breaks: without it a rebase reports every object git
        // writes under `.git`, and every server watching a repository with a vendored bundle pays
        // for each write git makes.
        //
        // Asserted by exhaustion, not by a timeout alone: a write the walk *does* cover is made
        // afterwards, which proves the watcher was awake and chose.
        let watched = watching(&[("app/models/story.rb", "class Story\nend\n")]);
        let root = watched.root();

        std::fs::create_dir_all(root.join(".git/objects")).expect("the directory");
        std::fs::write(root.join(".git/objects/abcdef"), "not ruby\n").expect("write");
        std::fs::write(
            root.join("app/models/story.rb"),
            "class Story\n  def title\n  end\nend\n",
        )
        .expect("write");

        let (arrived, seen) = watched.until("story.rb");
        assert!(arrived, "the watcher was awake and chose");
        assert!(
            !seen.iter().any(|uri| uri.contains("/.git/")),
            "the walk never went into .git and neither may the watcher: {seen:?}"
        );
    }
}
