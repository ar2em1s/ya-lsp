//! `ya-lsp coverage`: what share of the calls a project's own code makes ya-lsp can type.
//!
//! **The question is the one a hover asks**, asked in process: at a call,
//! what [`cursor::type_of`] → [`types::method_receiver`] says it returns, the reader the margin
//! uses for `x = call`. A call is *typed* where the answer is Resolved or Derived and can be
//! spelled, which is where that margin would draw a label; a guess, an unnameable type and no
//! answer are not.
//!
//! - **Which calls: the production code's, whose value is used.** The project's own Ruby files
//!   ([`environment::Layout::is_own`]) that the application loads ([`environment::Fence::unloadable`]:
//!   no test tree, migration or generator template, as `[trees]` says), so nothing here knows a
//!   framework. A call counts where something reads its value ([`used_calls`]); a statement's value,
//!   a class body's macro and a loop body are never read. Operators, setters and `foo.()` are left
//!   out: there is no name to ask about, or the value is the argument.
//! - **A sample, not a census.** [`SAMPLE`] calls drawn uniformly with a fixed seed, so a rerun on
//!   the same tree asks the same calls and says the same number. A project with no more calls than
//!   that is asked about every one, and its share is exact.
//! - **The error is the sample's, at 95%** ([`Coverage::margin`]): how far the share of all the
//!   calls may be from the sample's. It says nothing about how strict *typed* is.
//! - **It is its own server.** [`run`] loads the workspace and `ya-lsp.toml` as the server does,
//!   indexes the project and its bundle, runs the generator pass, then asks. No client is involved:
//!   what the server would say to one (`window/showMessage`) comes back as [`Step::Warning`].

use std::{io::Write, path::PathBuf};

use ruby_prism::{Node, Visit};
use rubydex::model::ids::UriId;

use super::{Analysis, Cancellations, ClientSupport, Stage, environment, types};
use crate::analysis::cursor;
use crate::analysis::position::PositionEncoding;
use crate::analysis::render;
use crate::workspace::{Workspace, gems, uri::DocUri};

/// How many calls a run asks about. 2,000 puts the error at ±2.2 points at worst (a share near
/// half) and takes a few seconds once the project is indexed.
pub const SAMPLE: usize = 2_000;

/// The draw's seed. Fixed, so two runs over one tree ask the same calls.
const SEED: u64 = 20_260_930;

/// The normal quantile of a two-sided 95% interval.
const Z: f64 = 1.96;

/// Where a run has got to, for the caller to show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// The project's own files are indexed.
    Project { files: usize },
    /// Signatures and gems going in, `done` of `total` files.
    Gems { done: usize, total: usize },
    /// Everything is indexed; the generator pass is running.
    Declaring,
    /// The calls the sample is drawn from.
    Found { calls: usize, files: usize },
    /// `done` of the sampled calls asked so far.
    Checked { done: usize, total: usize },
    /// What the server would have shown a client: a setting it could not read, a bundle it could
    /// not resolve.
    Warning(String),
}

/// What a run found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Coverage {
    /// Sampled calls ya-lsp typed.
    pub typed: usize,
    /// Calls sampled.
    pub asked: usize,
    /// Sampled calls that reach a framework's macro, whose value nothing reads after all
    /// ([`types::discarded`]): out of the share.
    pub discarded: usize,
    /// Calls the sample was drawn from.
    pub calls: usize,
}

impl Coverage {
    /// The sampled calls the share is of: those whose value is read.
    #[must_use]
    pub fn counted(self) -> usize {
        self.asked - self.discarded
    }

    /// The calls the share stands for: the population less the macro calls the sample found, in
    /// proportion (all of them where every call was asked).
    #[must_use]
    pub fn standing(self) -> usize {
        if self.asked == 0 {
            return self.calls;
        }
        let kept = self.counted() as f64 / self.asked as f64;
        (self.calls as f64 * kept).round() as usize
    }

    /// The sampled share typed, in percent. `None` where there was nothing to ask.
    #[must_use]
    pub fn percent(self) -> Option<f64> {
        let counted = self.counted();
        (counted > 0).then(|| 100.0 * self.typed as f64 / counted as f64)
    }

    /// Half the 95% interval around [`Self::percent`], in points. `None` where every call was
    /// asked, so the share is exact.
    ///
    /// - **Agresti–Coull**: the share is taken as `(typed + 2) / (asked + 4)` for the width only.
    ///   A plain `p(1 - p)` says ±0 for a sample typed all or none, which a sample never proves.
    /// - **With the finite-population correction**: a sample that is most of a small project's
    ///   calls leaves less unknown, and one that is all of them leaves nothing.
    #[must_use]
    pub fn margin(self) -> Option<f64> {
        if self.counted() == 0 || self.asked >= self.calls {
            return None;
        }
        let asked = self.counted() as f64;
        let calls = self.standing() as f64;
        let share = (self.typed as f64 + 2.0) / (asked + 4.0);
        let finite = ((calls - asked) / (calls - 1.0)).sqrt();
        Some(100.0 * Z * (share * (1.0 - share) / (asked + 4.0)).sqrt() * finite)
    }
}

/// Index the project at `root` as the server would, with `env` for gem discovery, and ask `size`
/// calls.
///
/// On a thread of the analysis thread's size: typing a call recurses as deep as a hover does.
fn run(root: PathBuf, env: gems::Env, size: usize, say: &mut (dyn FnMut(Step) + Send)) -> Coverage {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("ya-lsp-coverage".to_owned())
            .stack_size(super::ANALYSIS_STACK)
            .spawn_scoped(scope, move || measure(root, env, size, say))
            .expect("failed to spawn the coverage thread")
            .join()
            .expect("the coverage thread panicked")
    })
}

/// `ya-lsp coverage [DIR]`, with `DIR` the arguments after the word: progress on `err`, the one
/// result line on `out`. Returns whether it measured.
///
/// **`terminal` says whether `err` is one**: a terminal gets a counter rewritten in place for the
/// two long phases, anything else one line per phase, so a CI log is not two thousand lines long.
pub fn command(
    arguments: &[String],
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
    terminal: bool,
) -> bool {
    let root = directory(arguments, std::env::current_dir());
    command_with(root, out, err, terminal, gems::Env::from_process(), SAMPLE)
}

/// The project a run measures: the one argument, else the current directory (`current`, read at
/// the edge), or what is wrong with the arguments.
fn directory(arguments: &[String], current: std::io::Result<PathBuf>) -> Result<PathBuf, String> {
    let root = match arguments {
        [] => current.map_err(|error| format!("cannot read the current directory: {error}"))?,
        [directory] if !directory.starts_with('-') => std::path::absolute(directory)
            .map_err(|error| format!("cannot read {directory:?}: {error}"))?,
        _ => return Err("usage: ya-lsp coverage [DIR]".to_owned()),
    };
    if root.is_dir() {
        Ok(root)
    } else {
        Err(format!("{} is not a directory", root.display()))
    }
}

/// [`command`] over the project [`directory`] chose, with the environment gem discovery sees and
/// the sample's size supplied explicitly.
fn command_with(
    root: Result<PathBuf, String>,
    out: &mut dyn Write,
    err: &mut (dyn Write + Send),
    terminal: bool,
    env: gems::Env,
    size: usize,
) -> bool {
    let root = match root {
        Ok(root) => root,
        Err(problem) => {
            let _ = writeln!(err, "ya-lsp: {problem}");
            return false;
        }
    };
    let mut progress = Printer {
        err,
        terminal,
        open: false,
    };
    let coverage = run(root.clone(), env, size, &mut |step| progress.say(step));
    progress.close();
    let Some(percent) = coverage.percent() else {
        let _ = writeln!(
            progress.err,
            "ya-lsp: no calls found in {}'s own Ruby outside its test and migration folders",
            root.display()
        );
        return false;
    };
    let result = match coverage.margin() {
        Some(margin) => format!(
            "Type coverage: {percent:.1}% ± {margin:.1}% ({} of {} sampled)",
            grouped(coverage.counted()),
            counted(coverage.standing(), "call")
        ),
        None => format!(
            "Type coverage: {percent:.1}% (all {})",
            counted(coverage.standing(), "call")
        ),
    };
    let _ = writeln!(out, "{result}");
    true
}

/// [`Step`]s written out for a person.
struct Printer<'a> {
    err: &'a mut (dyn Write + Send),
    terminal: bool,
    /// A counter line is on screen without its line break.
    open: bool,
}

impl Printer<'_> {
    fn say(&mut self, step: Step) {
        match step {
            Step::Project { files } => {
                self.line(&format!("Indexed {}.", counted(files, "project file")));
            }
            Step::Gems { done, total } if self.terminal => self.counter(&format!(
                "Indexing gems and signatures: {} of {}",
                grouped(done),
                counted(total, "file")
            )),
            Step::Gems { done: 0, total } => self.line(&format!(
                "Indexing {}.",
                counted(total, "gem and signature file")
            )),
            Step::Declaring => self.line("Finishing the index."),
            Step::Found { calls, files } => self.line(&format!(
                "Found {} in {}.",
                counted(calls, "call"),
                counted(files, "file")
            )),
            Step::Checked { done, total } if self.terminal => self.counter(&format!(
                "Checking {}: {} done",
                counted(total, "call"),
                grouped(done)
            )),
            Step::Checked { done: 0, total } => {
                self.line(&format!("Checking {}.", counted(total, "call")));
            }
            Step::Warning(message) => self.line(&format!("warning: {message}")),
            Step::Gems { .. } | Step::Checked { .. } => {}
        }
    }

    /// One line of its own, below any counter.
    fn line(&mut self, text: &str) {
        self.close();
        let _ = writeln!(self.err, "{text}");
    }

    /// Rewrite the counter line in place.
    fn counter(&mut self, text: &str) {
        let _ = write!(self.err, "\r{text}\x1b[K");
        let _ = self.err.flush();
        self.open = true;
    }

    /// End a counter line, if one is open.
    fn close(&mut self) {
        if std::mem::take(&mut self.open) {
            let _ = writeln!(self.err);
        }
    }
}

/// `27855 calls`, `1 call`.
fn counted(count: usize, noun: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{} {noun}{plural}", grouped(count))
}

/// `27855` as `27,855`.
fn grouped(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn measure(
    root: PathBuf,
    env: gems::Env,
    size: usize,
    say: &mut (dyn FnMut(Step) + Send),
) -> Coverage {
    let (workspace, problems) = Workspace::load_with_env(root, None, env);
    for problem in problems {
        say(Step::Warning(problem));
    }
    let (outgoing, sent) = crossbeam_channel::unbounded();
    let mut analysis = Analysis::new(
        workspace,
        PositionEncoding::default(),
        // A client that takes nothing: no progress stream, no registration.
        ClientSupport::default(),
        Box::new(|_| Vec::new()),
        outgoing,
        Cancellations::default(),
        crate::logging::Reload::default(),
    );
    let warn = |say: &mut (dyn FnMut(Step) + Send)| {
        for message in sent.try_iter().filter_map(shown) {
            say(Step::Warning(message));
        }
    };
    analysis.index_workspace();
    warn(say);
    say(Step::Project {
        files: analysis.workspace_files,
    });
    analysis.queue_background_indexing();
    warn(say);
    loop {
        match &analysis.stage {
            Stage::Bundle(indexing) => say(Step::Gems {
                done: indexing.total - indexing.remaining.len(),
                total: indexing.total,
            }),
            Stage::Generate => say(Step::Declaring),
            Stage::Workspace | Stage::Ready => {}
        }
        if !analysis.step_pipeline() {
            break;
        }
        warn(say);
    }
    analysis.settle();
    warn(say);
    analysis.coverage(size, say)
}

/// What a `window/showMessage` the server sent says; `None` for anything else it sent.
fn shown(message: lsp_server::Message) -> Option<String> {
    let value = serde_json::to_value(message).ok()?;
    (value.get("method")?.as_str()? == "window/showMessage").then_some(())?;
    Some(value.pointer("/params/message")?.as_str()?.to_owned())
}

impl Analysis {
    /// Draw the sample from the production code's used calls and ask each one.
    fn coverage(&self, size: usize, say: &mut (dyn FnMut(Step) + Send)) -> Coverage {
        let layout = self.layout();
        // No cursor: the raw reading of each path, which is what `unloadable` answers.
        let fence = environment::Fence::at(None, layout);
        let mut files: Vec<DocUri> = self
            .graph
            .documents()
            .values()
            .map(|document| document.uri())
            .filter(|uri| uri.ends_with(".rb") && layout.is_own(uri) && !fence.unloadable(uri))
            .filter_map(DocUri::from_graph_uri)
            .collect();
        files.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        // Every used call, as its file's index and its message's offset, in file order.
        let calls: Vec<(usize, u32)> = files
            .iter()
            .enumerate()
            .flat_map(|(file, uri)| {
                self.with_text(uri, |text| used_calls(text.text()))
                    .unwrap_or_default()
                    .into_iter()
                    .map(move |offset| (file, offset))
            })
            .collect();
        say(Step::Found {
            calls: calls.len(),
            files: files.len(),
        });
        let sample = sample(calls.len(), size, SEED);
        let total = sample.len();
        let mut typed = 0;
        let mut discarded = 0;
        let mut done = 0;
        say(Step::Checked { done, total });
        // Grouped by file (the draw is sorted and the calls are in file order), so each text is
        // read and parsed once.
        for chunk in sample.chunk_by(|a, b| calls[*a].0 == calls[*b].0) {
            let uri = &files[calls[chunk[0]].0];
            let counted = self
                .with_text(uri, |text| {
                    let parsed = cursor::Parsed::new(text.text());
                    let rebase = self.rebase_for(uri, text.text());
                    chunk
                        .iter()
                        .map(|index| self.counted_at(uri, &parsed, &rebase, calls[*index].1))
                        .collect::<Vec<Counted>>()
                })
                .unwrap_or_default();
            typed += counted.iter().filter(|one| **one == Counted::Typed).count();
            discarded += counted
                .iter()
                .filter(|one| **one == Counted::Discarded)
                .count();
            done += chunk.len();
            say(Step::Checked { done, total });
        }
        Coverage {
            typed,
            asked: total,
            discarded,
            calls: calls.len(),
        }
    }

    /// What the call whose message starts at `offset` counts as: the probe's question, one call at
    /// a time with a fresh memo, as a request would ask it.
    ///
    /// - **Ruby's own `raise` and `fail` are typed**, as `bot`: they never return, which is all
    ///   there is to say of them, and their method's `!` says it ([`cursor::never_returns`]).
    /// - **A framework's macro is no used call** ([`types::discarded`]): its value is read by
    ///   nobody, wherever Ruby would hand it on.
    /// - **A panic is a call not typed**, contained here as `Analysis::serve` contains a request's:
    ///   one call rubydex cannot answer must not cost the run.
    fn counted_at(
        &self,
        uri: &DocUri,
        parsed: &cursor::Parsed<'_>,
        rebase: &super::position::Rebase,
        offset: u32,
    ) -> Counted {
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let read = |uri: &str| self.read_of(uri);
            let memo = types::Memo::new(&read, &self.exits);
            let sources = self.sources(&read, &memo);
            let uri_id = UriId::from(uri.as_str());
            let at = rebase.to_graph(offset)?;
            let (_, _, receiver) = cursor::type_of(parsed, offset)?;
            let receiver = receiver.rebased(rebase)?;
            if cursor::never_returns(&receiver) {
                return Some(Counted::Typed);
            }
            let scope = types::Scope::at(sources.graph, uri_id, at);
            if types::discarded(&sources, uri_id, &receiver, &scope) {
                return Some(Counted::Discarded);
            }
            let typed = types::method_receiver(&sources, uri_id, &receiver, &scope)?;
            let sure = matches!(
                typed.derivation.tier(),
                types::Tier::Resolved | types::Tier::Derived
            );
            let typed_here = sure
                && render::typed(sources.graph, &typed).is_some_and(|spelled| !spelled.is_empty());
            Some(if typed_here {
                Counted::Typed
            } else {
                Counted::Untyped
            })
        }))
        .ok()
        .flatten()
        .unwrap_or(Counted::Untyped)
    }
}

/// What one sampled call counts as ([`Analysis::counted_at`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Counted {
    Typed,
    Untyped,
    /// A framework's macro, whose value nothing reads: out of the share.
    Discarded,
}

/// `size` indices out of `0..population`, drawn uniformly without replacement and sorted: every
/// index where the population is no larger.
///
/// A partial Fisher–Yates shuffle over a SplitMix64 stream, written here rather than taken from a
/// crate: twenty lines, and the draw must not move under a dependency bump, or two runs over one
/// tree would stop comparing.
fn sample(population: usize, size: usize, seed: u64) -> Vec<usize> {
    if population <= size {
        return (0..population).collect();
    }
    let mut state = seed;
    let mut next = move || {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut mixed = state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        mixed ^ (mixed >> 31)
    };
    let mut indices: Vec<usize> = (0..population).collect();
    for drawn in 0..size {
        // The modulo's bias is below one part in 2^32 for any population a project has.
        let pick = drawn + (next() % (population - drawn) as u64) as usize;
        indices.swap(drawn, pick);
    }
    indices.truncate(size);
    indices.sort_unstable();
    indices
}

/// The byte offset of each call's message in `source` whose value something reads.
///
/// - **A call is a name a person can hover**: a message spelled as an identifier (`save`,
///   `valid?`, `sort!`). Operators, `[]`, a setter (`x = v` is `v`) and `foo.()` have none.
/// - **Used** is Ruby's reading of where a value goes. Every statement of a list but the last is
///   thrown away, and the last hands its value to whatever holds the list. A file, class or module
///   body's statements, a loop's body and an `ensure` are thrown away whole. A `def`'s last
///   statement is its return, except in `initialize` and a writer, whose value Ruby discards. A
///   block's last statement is read when the call it is given to is (`map` assigned: yes; `each`
///   as a statement: no). A branch, a `rescue` and the right side of `&&` hand their value on;
///   everything else (an argument, a receiver, a condition) is read.
#[must_use]
pub fn used_calls(source: &str) -> Vec<u32> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut walk = Used::default();
    walk.visit(&parsed.node());
    // In the order the source writes them, which the walk (a call before its receiver) is not.
    walk.found.sort_unstable();
    walk.found
}

/// [`used_calls`]' walk. `next` is what the parent says of the child it is about to visit; a
/// parent that says nothing reads its children. `used` holds each open node's own answer.
#[derive(Default)]
struct Used {
    next: Option<bool>,
    used: Vec<bool>,
    found: Vec<u32>,
}

impl Used {
    /// Whether the node being visited is read.
    fn here(&self) -> bool {
        self.used.last().copied().unwrap_or(true)
    }

    /// Visit `node`, which is read or not as `used` says.
    fn child(&mut self, used: bool, node: &Node<'_>) {
        self.next = Some(used);
        self.visit(node);
    }

    fn maybe(&mut self, used: bool, node: Option<Node<'_>>) {
        if let Some(node) = node {
            self.child(used, &node);
        }
    }
}

/// A name a call can be hovered on: an identifier, with an optional `?` or `!`.
fn is_identifier(name: &[u8]) -> bool {
    std::str::from_utf8(name).is_ok_and(|name| {
        let stem = name.strip_suffix(['?', '!']).unwrap_or(name);
        let mut chars = stem.chars();
        chars
            .next()
            .is_some_and(|first| first.is_alphabetic() || first == '_')
            && chars.all(|rest| rest.is_alphanumeric() || rest == '_')
    })
}

/// Whether Ruby reads what a `def` of this name returns: not `initialize`, and not a writer.
fn returns_a_value(name: &[u8]) -> bool {
    name != b"initialize"
        && (!name.ends_with(b"=") || matches!(name, b"==" | b"!=" | b"<=" | b">=" | b"==="))
}

impl<'pr> Visit<'pr> for Used {
    fn visit_branch_node_enter(&mut self, _node: Node<'pr>) {
        let used = self.next.take().unwrap_or(true);
        self.used.push(used);
    }

    fn visit_branch_node_leave(&mut self) {
        self.used.pop();
    }

    fn visit_leaf_node_enter(&mut self, _node: Node<'pr>) {
        self.next = None;
    }

    fn visit_program_node(&mut self, node: &ruby_prism::ProgramNode<'pr>) {
        self.child(false, &node.statements().as_node());
    }

    fn visit_statements_node(&mut self, node: &ruby_prism::StatementsNode<'pr>) {
        let used = self.here();
        let body: Vec<Node<'pr>> = node.body().iter().collect();
        let last = body.len().saturating_sub(1);
        for (at, statement) in body.iter().enumerate() {
            self.child(used && at == last, statement);
        }
    }

    fn visit_class_node(&mut self, node: &ruby_prism::ClassNode<'pr>) {
        self.child(true, &node.constant_path());
        self.maybe(true, node.superclass());
        self.maybe(false, node.body());
    }

    fn visit_module_node(&mut self, node: &ruby_prism::ModuleNode<'pr>) {
        self.child(true, &node.constant_path());
        self.maybe(false, node.body());
    }

    fn visit_singleton_class_node(&mut self, node: &ruby_prism::SingletonClassNode<'pr>) {
        self.child(true, &node.expression());
        self.maybe(false, node.body());
    }

    fn visit_def_node(&mut self, node: &ruby_prism::DefNode<'pr>) {
        self.maybe(true, node.receiver());
        self.maybe(
            true,
            node.parameters().map(|parameters| parameters.as_node()),
        );
        self.maybe(returns_a_value(node.name().as_slice()), node.body());
    }

    fn visit_call_node(&mut self, node: &ruby_prism::CallNode<'pr>) {
        let used = self.here();
        if used
            && let Some(message) = node.message_loc()
            && is_identifier(node.name().as_slice())
        {
            self.found.push(message.start_offset() as u32);
        }
        self.maybe(true, node.receiver());
        self.maybe(true, node.arguments().map(|arguments| arguments.as_node()));
        if let Some(block) = node.block() {
            let written = block.as_block_node().is_some();
            self.child(!written || used, &block);
        }
    }

    fn visit_block_node(&mut self, node: &ruby_prism::BlockNode<'pr>) {
        let used = self.here();
        self.maybe(true, node.parameters());
        self.maybe(used, node.body());
    }

    fn visit_if_node(&mut self, node: &ruby_prism::IfNode<'pr>) {
        let used = self.here();
        self.child(true, &node.predicate());
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
        self.maybe(used, node.subsequent());
    }

    fn visit_unless_node(&mut self, node: &ruby_prism::UnlessNode<'pr>) {
        let used = self.here();
        self.child(true, &node.predicate());
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
        self.maybe(used, node.else_clause().map(|clause| clause.as_node()));
    }

    fn visit_else_node(&mut self, node: &ruby_prism::ElseNode<'pr>) {
        let used = self.here();
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_case_node(&mut self, node: &ruby_prism::CaseNode<'pr>) {
        let used = self.here();
        self.maybe(true, node.predicate());
        for condition in node.conditions().iter() {
            self.child(used, &condition);
        }
        self.maybe(used, node.else_clause().map(|clause| clause.as_node()));
    }

    fn visit_case_match_node(&mut self, node: &ruby_prism::CaseMatchNode<'pr>) {
        let used = self.here();
        self.maybe(true, node.predicate());
        for condition in node.conditions().iter() {
            self.child(used, &condition);
        }
        self.maybe(used, node.else_clause().map(|clause| clause.as_node()));
    }

    fn visit_when_node(&mut self, node: &ruby_prism::WhenNode<'pr>) {
        let used = self.here();
        for condition in node.conditions().iter() {
            self.child(true, &condition);
        }
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_in_node(&mut self, node: &ruby_prism::InNode<'pr>) {
        let used = self.here();
        self.child(true, &node.pattern());
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_while_node(&mut self, node: &ruby_prism::WhileNode<'pr>) {
        self.child(true, &node.predicate());
        self.maybe(
            false,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_until_node(&mut self, node: &ruby_prism::UntilNode<'pr>) {
        self.child(true, &node.predicate());
        self.maybe(
            false,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_for_node(&mut self, node: &ruby_prism::ForNode<'pr>) {
        self.child(true, &node.index());
        self.child(true, &node.collection());
        self.maybe(
            false,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_begin_node(&mut self, node: &ruby_prism::BeginNode<'pr>) {
        let used = self.here();
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
        self.maybe(used, node.rescue_clause().map(|clause| clause.as_node()));
        self.maybe(used, node.else_clause().map(|clause| clause.as_node()));
        self.maybe(false, node.ensure_clause().map(|clause| clause.as_node()));
    }

    fn visit_ensure_node(&mut self, node: &ruby_prism::EnsureNode<'pr>) {
        let used = self.here();
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
    }

    fn visit_rescue_node(&mut self, node: &ruby_prism::RescueNode<'pr>) {
        let used = self.here();
        for exception in node.exceptions().iter() {
            self.child(true, &exception);
        }
        self.maybe(true, node.reference());
        self.maybe(
            used,
            node.statements().map(|statements| statements.as_node()),
        );
        self.maybe(used, node.subsequent().map(|clause| clause.as_node()));
    }

    fn visit_rescue_modifier_node(&mut self, node: &ruby_prism::RescueModifierNode<'pr>) {
        let used = self.here();
        self.child(used, &node.expression());
        self.child(used, &node.rescue_expression());
    }

    fn visit_parentheses_node(&mut self, node: &ruby_prism::ParenthesesNode<'pr>) {
        let used = self.here();
        self.maybe(used, node.body());
    }

    fn visit_and_node(&mut self, node: &ruby_prism::AndNode<'pr>) {
        let used = self.here();
        self.child(true, &node.left());
        self.child(used, &node.right());
    }

    fn visit_or_node(&mut self, node: &ruby_prism::OrNode<'pr>) {
        let used = self.here();
        self.child(true, &node.left());
        self.child(used, &node.right());
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// The names [`used_calls`] finds in `source`, in order.
    fn used_names(source: &str) -> Vec<&str> {
        used_calls(source)
            .into_iter()
            .map(|offset| {
                let rest = &source[offset as usize..];
                let end = rest
                    .find(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '?' | '!')))
                    .unwrap_or(rest.len());
                &rest[..end]
            })
            .collect()
    }

    #[test]
    fn a_call_counts_where_ruby_reads_its_value() {
        let source = "\
class Shelf
  has_many :books

  def title
    compute
    format(name.upcase)
  end

  def initialize(x)
    @x = build(x)
    setup
  end

  def name=(value)
    store(value)
  end

  def ==(other)
    same?(other)
  end

  def sorted
    list.map { |book| book.title }.sort
  end

  def printed
    items.each { |item| show(item) }
    nil
  end

  def waiting
    while ready?
      tick
    end
    until done!
      tock
    end
    for page in pages
      turn(page)
    end
  end

  def rescued
    fetch
  rescue failure_class => error
    fallback
  else
    otherwise
  ensure
    cleanup
  end

  def chosen(kind)
    case kind
    when first_kind then first
    else other
    end
  end

  def matched(value)
    case value
    in Integer then whole
    else part
    end
  end

  def guarded
    left && right
    either || neither
    (inner)
    maybe rescue plan_b
    ready ? yes : no
    unless off then on_branch else off_branch end
  end

  def added(a) = a + b.c

  def names = people.map(&:name)
end

class << registry
  entry
end

module Kit
  tool
end

obj.save
x = obj.reload
called = thing.()
thing[1]
-> { lambda_body }
begin
  quiet
end
";
        assert_eq!(
            used_names(source),
            [
                // `compute` is thrown away; the last statement is the return.
                "format",
                "name",
                "upcase",
                // `initialize` returns nothing anyone reads, but the value it assigns is read.
                "build",
                // A writer's return is discarded; `==` is an operator, and its return is read.
                "same?",
                // A block's value is read where its call's is (`map`'s is read by `sort`).
                "list",
                "map",
                "title",
                "sort",
                // `each` is a statement here, so neither it nor its block is read.
                "items",
                // Conditions are read, loop bodies are not.
                "ready?",
                "done!",
                "pages",
                // The body and every clause but `ensure` hand the value on.
                "fetch",
                "failure_class",
                "fallback",
                "otherwise",
                "first_kind",
                "first",
                "other",
                "whole",
                "part",
                // `left` is read to decide; `right` and the ternary's arms are dropped with their
                // statements; the last statement's branches are the return.
                "left",
                "either",
                "ready",
                "off",
                "on_branch",
                "off_branch",
                // An operator is no name to ask, but its operands are read, and so is a call a
                // block argument is handed to.
                "b",
                "c",
                "people",
                "map",
                // A body's statements are not read, the expression it opens on is. At the top
                // level: receivers, a value assigned, and a lambda's body.
                "registry",
                "obj",
                "obj",
                "reload",
                "thing",
                "thing",
                "lambda_body",
            ]
        );
    }

    #[test]
    fn the_draw_is_every_call_below_the_sample_and_the_same_every_time_above_it() {
        assert_eq!(sample(3, 5, SEED), [0, 1, 2]);
        assert_eq!(sample(0, 5, SEED), Vec::<usize>::new());
        let drawn = sample(10_000, 100, SEED);
        assert_eq!(drawn.len(), 100);
        assert!(
            drawn.windows(2).all(|pair| pair[0] < pair[1]),
            "sorted, distinct"
        );
        assert!(drawn.iter().all(|index| *index < 10_000));
        assert_eq!(
            drawn,
            sample(10_000, 100, SEED),
            "a rerun asks the same calls"
        );
        assert_ne!(drawn, sample(10_000, 100, SEED + 1));
    }

    #[test]
    fn the_error_is_the_samples_and_there_is_none_where_every_call_was_asked() {
        let near_half = Coverage {
            typed: 956,
            asked: 2_000,
            discarded: 0,
            calls: 27_855,
        };
        assert_eq!(format!("{:.1}", near_half.percent().unwrap()), "47.8");
        assert_eq!(format!("{:.1}", near_half.margin().unwrap()), "2.1");
        // Most of a small project asked leaves less unknown.
        let most = Coverage {
            typed: 956,
            asked: 2_000,
            discarded: 0,
            calls: 2_500,
        };
        assert!(most.margin().unwrap() < near_half.margin().unwrap() / 2.0);
        // A sample typed throughout still has an error.
        let all = Coverage {
            typed: 2_000,
            asked: 2_000,
            discarded: 0,
            calls: 27_855,
        };
        assert!(all.margin().unwrap() > 0.0);
        let census = Coverage {
            typed: 3,
            asked: 4,
            discarded: 0,
            calls: 4,
        };
        assert_eq!(census.percent(), Some(75.0));
        assert_eq!(census.margin(), None);
        let nothing = Coverage {
            typed: 0,
            asked: 0,
            discarded: 0,
            calls: 0,
        };
        assert_eq!(nothing.percent(), None);
        assert_eq!(nothing.margin(), None);
        assert_eq!(nothing.standing(), 0);
        // A macro call the sample found is out of the share, and out of the calls in proportion.
        let macros = Coverage {
            typed: 956,
            asked: 2_000,
            discarded: 400,
            calls: 27_855,
        };
        assert_eq!(macros.counted(), 1_600);
        assert_eq!(macros.standing(), 22_284);
        assert_eq!(format!("{:.1}", macros.percent().unwrap()), "59.8");
        assert!(macros.margin().unwrap() > near_half.margin().unwrap());
        // Every sampled call a macro's: nothing to say.
        let only_macros = Coverage {
            typed: 0,
            asked: 4,
            discarded: 4,
            calls: 4,
        };
        assert_eq!(only_macros.percent(), None);
        assert_eq!(only_macros.margin(), None);
    }

    #[test]
    fn a_count_is_grouped_in_thousands() {
        for (count, written) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (27_855, "27,855"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(grouped(count), written);
        }
    }

    /// Runs `ya-lsp coverage` on `root` in process, asking `size` calls: what it printed to stdout,
    /// to stderr, and whether it measured.
    fn command_at(
        root: &std::path::Path,
        terminal: bool,
        env: gems::Env,
        size: usize,
    ) -> (String, String, bool) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let measured = command_with(
            Ok(root.to_path_buf()),
            &mut out,
            &mut err,
            terminal,
            env,
            size,
        );
        (
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
            measured,
        )
    }

    /// A Ruby project with no framework, a suite and a migration folder, and one gem whose class
    /// its own code calls.
    fn project() -> (tempfile::TempDir, tempfile::TempDir, gems::Env) {
        let (dir, elsewhere, env) = crate::analysis::testing::project_with_gem_file(
            "lib/shouty.rb",
            "class Shouty\n  def loud\n    Loud.new\n  end\nend\n\nclass Loud\nend\n",
        );
        let root = dir.path();
        let write = |relative: &str, source: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        };
        write(
            "ya-lsp.toml",
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n[index]\nexclude = [\"a[b\"]\n",
        );
        write(
            "lib/greeter.rb",
            "\
class Greeter
  def greet
    Greeting.new
  end

  def text
    greet.words
  end

  def shout
    Shouty.new.loud
  end

  def unknown(x)
    x.anything
  end

  def guessed
    @greeting.words
  end
end

class Greeting
  def words
    Words.new
  end
end

class Words
end
",
        );
        write("spec/greeter_spec.rb", "Greeter.new.greet.words\n");
        write(
            "db/migrate/001_greet.rb",
            "def up\n  Greeter.new.greet\nend\n",
        );
        (dir, elsewhere, env)
    }

    #[test]
    fn a_project_with_no_framework_is_measured_on_its_own_code_alone() {
        let (dir, _elsewhere, env) = project();
        let (out, err, measured) = command_at(dir.path(), false, env, SAMPLE);
        assert!(measured, "{err}");
        // `new`, `greet`, `words`, `new`, `loud` and `new` are typed, the gem's class among them;
        // `x.anything` is not, and `@greeting.words` is only a name's guess. The suite and the
        // migration are not the project's production code.
        assert_eq!(out, "Type coverage: 75.0% (all 8 calls)\n");
        let lines: Vec<&str> = err.lines().collect();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("warning: ") && line.contains("a[b")),
            "a setting it could not read is said: {err}"
        );
        for said in [
            "Indexed 3 project files.",
            "Indexing 1 gem and signature file.",
            "Finishing the index.",
            "Found 8 calls in 1 file.",
            "Checking 8 calls.",
        ] {
            assert!(lines.contains(&said), "{said}: {err}");
        }
    }

    #[test]
    fn a_raise_is_typed_and_a_macro_s_value_is_no_used_call() {
        let (dir, _elsewhere, env) =
            crate::analysis::testing::project_with_gem_file("lib/shouty.rb", "class Shouty\nend\n");
        let root = dir.path();
        let write = |relative: &str, source: &str| {
            let path = root.join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, source).unwrap();
        };
        write(
            "ya-lsp.toml",
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n",
        );
        write(
            "lib/story.rb",
            "\
module ActiveRecord
  class Base
    def self.belongs_to(name)
      name
    end
  end
end

class Story < ActiveRecord::Base
  def self.relate
    belongs_to(:author)
  end

  def check
    raise(\"no\")
  end

  def copy
    Story.new
  end

  def broken
    Story.new.nothing
  end
end
",
        );
        let (out, err, measured) = command_at(root, false, env, SAMPLE);
        assert!(measured, "{err}");
        // `belongs_to` is Rails' macro, which nothing reads; `raise` never returns, each `new` is a
        // `Story`, and a `Story` has no `nothing`.
        assert!(
            err.lines().any(|line| line == "Found 5 calls in 1 file."),
            "{err}"
        );
        assert_eq!(out, "Type coverage: 75.0% (all 4 calls)\n");
    }

    #[test]
    fn a_sample_smaller_than_the_calls_says_its_error() {
        let (dir, _elsewhere, env) = project();
        let (out, err, measured) = command_at(dir.path(), false, env, 2);
        assert!(measured, "{err}");
        assert!(
            out.starts_with("Type coverage: ") && out.ends_with("% (2 of 8 calls sampled)\n"),
            "{out}"
        );
        assert!(out.contains("% ± "), "{out}");
    }

    #[test]
    fn a_terminal_gets_counters_rewritten_in_place() {
        let (dir, _elsewhere, env) = project();
        let (out, err, measured) = command_at(dir.path(), true, env, SAMPLE);
        assert!(measured, "{err}");
        assert!(out.starts_with("Type coverage: "), "{out}");
        assert!(
            err.contains("\rIndexing gems and signatures: 0 of 1 file\x1b[K"),
            "{err:?}"
        );
        assert!(
            err.contains("\rChecking 8 calls: 8 done\x1b[K\n"),
            "the counter's line ends before the next: {err:?}"
        );
    }

    #[test]
    fn a_run_that_finds_no_calls_says_so_and_fails() {
        let empty = tempfile::tempdir().unwrap();
        let write = |relative: &str, source: &str| {
            std::fs::write(empty.path().join(relative), source).unwrap();
        };
        // One file over the budget, so the server says so the way it tells a client.
        write(
            "ya-lsp.toml",
            "[gems]\ndefault_gems = false\n\n[rbs]\nenabled = false\n\n[index]\nmax_files = 1\n",
        );
        write("a.rb", "first\n");
        write("b.rb", "second\n");
        let (out, err, measured) = command_at(empty.path(), false, gems::Env::default(), SAMPLE);
        assert!(!measured);
        assert!(out.is_empty());
        assert!(
            err.lines().any(|line| line.starts_with("warning: ")),
            "what the server shows a client is said: {err}"
        );
        assert!(err.contains("ya-lsp: no calls found in "), "{err}");
    }

    #[test]
    fn the_project_is_the_one_argument_or_where_the_command_runs() {
        let here = tempfile::tempdir().unwrap();
        let root = here.path().to_path_buf();
        let file = root.join("a.rb");
        std::fs::write(&file, "").unwrap();
        let given = |arguments: &[&str], current: std::io::Result<PathBuf>| {
            let arguments: Vec<String> = arguments
                .iter()
                .map(|&argument| argument.to_owned())
                .collect();
            directory(&arguments, current)
        };
        assert_eq!(given(&[], Ok(root.clone())), Ok(root.clone()));
        assert_eq!(
            given(&[], Err(std::io::Error::other("gone"))),
            Err("cannot read the current directory: gone".to_owned())
        );
        assert_eq!(
            given(
                &[root.to_str().unwrap()],
                Err(std::io::Error::other("unread"))
            ),
            Ok(root.clone())
        );
        assert_eq!(
            given(&[file.to_str().unwrap()], Ok(root.clone())),
            Err(format!("{} is not a directory", file.display()))
        );
        assert!(
            given(&[""], Ok(root.clone()))
                .is_err_and(|problem| problem.starts_with("cannot read \"\""))
        );
        for arguments in [&["a", "b"][..], &["--sample"][..]] {
            assert_eq!(
                given(arguments, Ok(root.clone())),
                Err("usage: ya-lsp coverage [DIR]".to_owned())
            );
        }

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let measured = command_with(
            Err("usage: ya-lsp coverage [DIR]".to_owned()),
            &mut out,
            &mut err,
            false,
            gems::Env::default(),
            SAMPLE,
        );
        assert!(!measured);
        assert!(out.is_empty());
        assert_eq!(
            String::from_utf8(err).unwrap(),
            "ya-lsp: usage: ya-lsp coverage [DIR]\n"
        );
    }
}
