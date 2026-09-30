//! RSpec: the example groups a spec file writes, what `let` and `subject` return, and what `self`
//! is in each block.
//!
//! The reading is `workspace::rspec`'s; this is the orchestration around it, and the proof that a
//! second body of knowledge is a module of its own: it touches no file outside this one but the
//! registry and the feature key.
//!
//! **Its documents live in a tree `environment.rs` fences**, and that already works: a cursor
//! inside `spec/` turns the fence off, so `let(:user)` answers inside a spec without touching it.
//!
//! **Its declarations have no named owner.** `RSpec.describe Foo do` is an anonymous subclass, so
//! `workspace::rspec` names one per group, the way the pass already mints a relation class.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{Counted, Declared, Declaring, Fresh, ListId, Sources, Wants};
use crate::generated::candidates;
use crate::workspace::{DocUri, Features, rspec};

/// Files whose name ends `_spec.rb`.
pub const GROUPS: ListId = ListId("rspec.groups");

/// Files that name `RSpec` or define a shared group: where a project's `RSpec.configure` blocks
/// and its support files' shared groups are.
pub const CONFIGS: ListId = ListId("rspec.configs");

static WANTS: [Wants; 2] = [
    Wants {
        list: GROUPS,
        calls: &[],
        constants: &[],
        modules: &[],
        defines: &[],
        path: Some("_spec.rb"),
        spells: &[],
        tags: false,
        inherits: false,
        // a gem's own specs are about a project that is not this one: `is_own_code`'s sentence, read
        // straight
        engines: false,
        gems: false,
        reads_only: false,
        // A closed spec file declares nothing: see [`RSpec::refresh`].
        buffers: true,
    },
    Wants {
        list: CONFIGS,
        // A support file defining shared groups at its top without naming `RSpec`.
        calls: &["shared_examples", "shared_context", "shared_examples_for"],
        // `TestProf`: where a project registers a `let_it_be` modifier of its own.
        constants: &["RSpec", "TestProf"],
        modules: &[],
        defines: &[],
        path: None,
        spells: &[],
        tags: false,
        inherits: false,
        engines: false,
        gems: false,
        // What these say is written on the groups and on rspec-core's class, not on them.
        reads_only: true,
        buffers: false,
    },
];

/// The example groups every spec file writes, parsed once per version of its text.
#[derive(Debug, Default)]
pub struct RSpec {
    /// Each spec file's reading, by graph URI, with what identifies the text it was read from.
    sources: BTreeMap<String, Held>,
    /// Each configuration file's `RSpec.configure` blocks and top-level shared groups, by graph
    /// URI.
    configs: BTreeMap<String, Support>,
    /// How many files were parsed, for a test that asserts an unchanged one is not.
    pub reads: usize,
}

/// One file that names `RSpec` without being a spec, read.
#[derive(Debug)]
struct Support {
    fresh: Fresh,
    caption: String,
    configured: Arc<rspec::Configured>,
    /// Its groups and shared groups; only the top-level shared ones are read.
    spec: Arc<rspec::Spec>,
}

/// One spec file, read.
#[derive(Debug)]
struct Held {
    fresh: Fresh,
    caption: String,
    spec: Arc<rspec::Spec>,
}

impl super::Knowledge for RSpec {
    fn name(&self) -> &'static str {
        "rspec"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, list: ListId, features: Features) -> bool {
        (list == GROUPS || list == CONFIGS) && features.rspec
    }

    fn spellable_names(&self) -> Vec<&'static str> {
        rspec::CONSTANTS.to_vec()
    }

    /// The gems' own `def` of a DSL or syntax member this module declared without a place.
    fn written_in(&self, owner: &str, method: &str) -> Option<String> {
        rspec::written_in(owner, method)
    }

    /// Parse every spec file the editor holds whose text moved, and forget the rest.
    ///
    /// - **Only the files the editor holds** ([`Sources::held`]). A group's classes are that
    ///   file's alone: nothing outside a spec file names them, so a closed one's would be declared
    ///   for no reader, and a large suite's are tens of thousands of classes. Opening a file puts
    ///   it on the pass's gate like any listed document, and closing one drops what it implied.
    /// - **The texts are read here and parsed on every core at once**: each parse is independent
    ///   and pure.
    fn refresh(&mut self, sources: &Sources<'_>) {
        let mut wanted: BTreeMap<&String, Fresh> = BTreeMap::new();
        for key in sources.context.documents(GROUPS) {
            // Asked before `fresh`, which would read every closed spec file's modification time.
            if let Some(uri) = DocUri::from_graph_uri(key).filter(|uri| (sources.held)(uri)) {
                wanted.insert(key, (sources.fresh)(&uri));
            }
        }
        self.sources.retain(|uri, _| wanted.contains_key(uri));
        let mut moved: Vec<(String, Fresh, String, String)> = Vec::new();
        for (key, fresh) in wanted {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            if self
                .sources
                .get(key)
                .is_some_and(|held| held.fresh == fresh)
            {
                continue;
            }
            let Some(text) = (sources.text)(&uri) else {
                self.sources.remove(key);
                continue;
            };
            moved.push((key.clone(), fresh, (sources.caption)(&uri), text));
        }
        self.refresh_configs(sources);
        self.reads += moved.len();
        for ((key, fresh, caption, _), spec) in moved.iter().zip(read_all(&moved)) {
            self.sources.insert(
                key.clone(),
                Held {
                    fresh: *fresh,
                    caption: caption.clone(),
                    spec: Arc::new(spec),
                },
            );
        }
    }

    /// Every spec file's groups, then the DSL's own signatures on rspec-core's classes.
    ///
    /// Nothing where the bundle does not declare `RSpec::ExampleGroups`: every name here is
    /// written under it, and rspec-core is not indexed.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        let namespaces = &declaring.context.namespaces;
        if !declaring.features.rspec || !namespaces.declares(rspec::EXAMPLE_GROUPS) {
            return Counted::new();
        }
        let resolve = |nesting: &str, constant: &str| {
            let found = if nesting.is_empty() {
                vec![constant.to_owned()]
            } else {
                candidates(nesting, constant)
            };
            found
                .into_iter()
                .find(|name| namespaces.declares(name))
                .map(|name| rspec::Resolved {
                    module: namespaces.opens(&name),
                    name,
                })
        };
        // One question to the bundle for every module a group may take and every class the DSL is
        // written on: it is a walk of every definition.
        let mut configured = rspec::Configured::default();
        for support in self.configs.values() {
            let read = &support.configured;
            configured.mixins.extend(read.mixins.iter().cloned());
            configured.infers_types |= read.infers_types;
            configured.mocks_elsewhere |= read.mocks_elsewhere;
            configured.expects_elsewhere |= read.expects_elsewhere;
            configured.modifiers |= read.modifiers;
        }
        // Two files whose paths spell one constant take it in URI order, the second suffixed as
        // RSpec suffixes a group: an answer must not depend on which file was read first.
        let mut taken: BTreeSet<String> = BTreeSet::new();
        let mut module_of = |caption: &str| {
            let base = rspec::file_module(caption);
            let mut module = base.clone();
            let mut next = 2;
            while !taken.insert(module.clone()) {
                module = format!("{base}_{next}");
                next += 1;
            }
            module
        };
        // A support file's shared groups are every spec's to include, so they are declared
        // whichever files are open.
        let mut registry = Vec::new();
        for (key, support) in &self.configs {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            if support
                .spec
                .shared
                .iter()
                .all(|shared| shared.parent.is_some())
            {
                continue;
            }
            let (facts, shared) = rspec::support_facts(
                &support.spec,
                &module_of(&support.caption),
                &support.caption,
                configured.modifiers,
            );
            registry.extend(shared);
            super::add(into, &uri, facts);
        }
        let hosts = (declaring.declares)(
            &rspec::HOSTS
                .iter()
                .map(|owner| (*owner).to_owned())
                .chain(rspec::wanted_modules(&configured))
                .collect(),
        );
        let setup = rspec::setup(
            &configured,
            &|name| hosts.contains_key(name) || namespaces.declares(name),
            registry,
        );
        let (mut groups, mut lets) = (0, 0);
        for (key, held) in &self.sources {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            let module = module_of(&held.caption);
            groups += held.spec.groups.len();
            lets += held
                .spec
                .groups
                .iter()
                .map(|group| group.lets.len())
                .sum::<usize>();
            super::add(
                into,
                &uri,
                rspec::spec_facts(&held.spec, &module, &held.caption, &setup, &resolve),
            );
        }
        for (owner, facts) in rspec::dsl(namespaces, &setup) {
            if let Some(uri) = hosts.get(owner) {
                super::add(into, uri, facts);
            }
        }
        vec![("example groups", groups), ("lets and subjects", lets)]
    }
}

impl RSpec {
    /// Read the `RSpec.configure` blocks of every file that names `RSpec`, spec files aside: they
    /// describe, and a spec file configuring the suite would configure it only once it had loaded.
    fn refresh_configs(&mut self, sources: &Sources<'_>) {
        let wanted: BTreeSet<&String> = sources
            .context
            .documents(CONFIGS)
            .iter()
            .filter(|key| !key.ends_with("_spec.rb"))
            .collect();
        self.configs.retain(|key, _| wanted.contains(key));
        for key in wanted {
            let Some(uri) = DocUri::from_graph_uri(key) else {
                continue;
            };
            let fresh = (sources.fresh)(&uri);
            if self
                .configs
                .get(key)
                .is_some_and(|held| held.fresh == fresh)
            {
                continue;
            }
            let Some(text) = (sources.text)(&uri) else {
                self.configs.remove(key);
                continue;
            };
            self.reads += 1;
            self.configs.insert(
                key.clone(),
                Support {
                    fresh,
                    caption: (sources.caption)(&uri),
                    configured: Arc::new(rspec::read_configured(&text)),
                    spec: Arc::new(rspec::read_spec(&text)),
                },
            );
        }
    }
}

/// Every text read, in order, spread over the machine's cores.
///
/// **One answer per text, whatever happens**: the caller pairs them by position, so a worker whose
/// reader panicked answers an empty reading for each of its texts rather than none, which would
/// hand one file's groups to another.
fn read_all(moved: &[(String, Fresh, String, String)]) -> Vec<rspec::Spec> {
    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZero::get);
    let chunk = moved.len().div_ceil(threads).max(1);
    std::thread::scope(|scope| {
        let workers: Vec<_> = moved
            .chunks(chunk)
            .map(|chunk| {
                let worker = scope.spawn(move || {
                    chunk
                        .iter()
                        .map(|(_, _, _, text)| rspec::read_spec(text))
                        .collect::<Vec<_>>()
                });
                (chunk.len(), worker)
            })
            .collect();
        workers
            .into_iter()
            .flat_map(|(texts, worker)| {
                worker
                    .join()
                    .unwrap_or_else(|_| vec![rspec::Spec::default(); texts])
            })
            .collect()
    })
}
