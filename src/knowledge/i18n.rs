//! The project's translations: which key a `t("…")` call names, what it holds in the main locale,
//! and where it is written.
//!
//! The reading is `workspace::i18n`'s; this finds the locale files, reads each once per version of
//! its text, merges them in i18n's load order, and answers the key hooks the analysis asks
//! ([`super::Knowledge::keyed_type`] and its siblings). The only RBS it writes says which members
//! look a key up (`generated::KEYED`), and a few of i18n's own returns.
//!
//! # Which files, in which order
//!
//! i18n's load path, as the Rails railtie builds it, later files winning:
//!
//! 1. **Rails' own**: `active_support`, `active_model`, `active_record` and `action_view` each ship
//!    `lib/<name>/locale/<locale>.yml`.
//! 2. **Every gem's `config/locales/**`**: an engine's, which the railtie puts before the
//!    application's.
//! 3. **The project's**: every `config/locales` it holds (in-repo engines and plugins first, the
//!    application's own last), or what `[i18n] paths` lists instead.
//!
//! **Only the main locale's files are read**: a file's locale is its top-level key, found without
//! parsing it ([`i18n::locales_in`]). The list is walked once and kept; a watched change under a
//! `config/locales` directory walks it again ([`super::Knowledge::touched`]).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::{
    Counted, Declared, Declaring, Fresh, Keyed, KeyedChild, KeyedEntry, ListId, Reading, Sources,
    Wants, Written,
};
use crate::generated::{Declared as Row, Facts, KEYED, Owner, Source};
use crate::workspace::i18n::{self, Asked, Entry, Held, LOOKUPS, RETURNS, Returned, Translations};
use crate::workspace::{DocUri, Features};

/// Directories no locale the project loads is in.
const SKIPPED: [&str; 12] = [
    ".git",
    "node_modules",
    "tmp",
    "log",
    "vendor",
    "public",
    "coverage",
    "spec",
    "test",
    "tests",
    "features",
    "target",
];

/// What this module keeps between passes.
#[derive(Debug, Default)]
pub struct Translate {
    /// What the last walk found, behind a lock because [`super::Knowledge::discover`] is `&self`.
    walked: Mutex<Walk>,
    /// The main-locale files, as core handed them back.
    found: Vec<DocUri>,
    /// Each file read, by URI: its freshness and its keys.
    read: HashMap<String, (Fresh, BTreeMap<String, Entry>)>,
    /// Every file's keys merged in load order.
    table: Arc<Translations>,
    /// How many files were parsed, for a test that asserts an unchanged one is not.
    pub reads: usize,
}

/// What a walk was for: the root, the gems' roots, the main locale and `[i18n] paths`.
type Walked = (PathBuf, Vec<PathBuf>, String, Option<Vec<String>>);

/// A file's modification time and length, `None` where it is gone.
type Stamp = Option<(SystemTime, u64)>;

/// The walk's memo.
#[derive(Debug, Default)]
struct Walk {
    /// What it was walked for.
    key: Option<Walked>,
    /// Every candidate file, in load order.
    files: Vec<PathBuf>,
    /// Each file's locales, and the stamp they were read at.
    locales: HashMap<PathBuf, (Stamp, Vec<String>)>,
}

impl Translate {
    /// Every candidate file, in load order: Rails' own, then every gem's `config/locales`, then
    /// the project's (engines and plugins before the application's own `config/locales`).
    fn candidates(reading: &Reading<'_>) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for gem in reading.gems {
            if let Some((_, directory)) = i18n::RAILS_OWN.iter().find(|(name, _)| *name == gem.name)
            {
                files.extend(files_in(&gem.path.join(directory), false));
            }
        }
        for gem in reading.gems {
            files.extend(files_in(&gem.path.join("config").join("locales"), true));
        }
        let mut own = match &reading.i18n.paths {
            Some(patterns) => patterns
                .iter()
                .flat_map(|pattern| {
                    glob::glob(&reading.root.join(pattern).to_string_lossy())
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                })
                .filter(|path| is_locale_file(path))
                .collect(),
            None => {
                let mut found = Vec::new();
                locale_directories(reading.root, 0, &mut found);
                found
                    .iter()
                    .flat_map(|directory| files_in(directory, true))
                    .collect::<Vec<_>>()
            }
        };
        // A `.rb` must be one `index.include` admits. The extension first: `admits` compiles the
        // globs and checks every ancestor, which over one corpus's 4,238 files cost 2.5 s a walk.
        own.retain(|path| path.extension().is_some_and(|x| x == "yml") || (reading.admits)(path));
        // The application's own `config/locales` last: it outranks what an engine it holds writes.
        let root_locales = reading.root.join("config").join("locales");
        own.sort_by_key(|path| (path.starts_with(&root_locales), path.clone()));
        files.extend(own);
        files
    }

    /// The main locale's files among the candidates, walking again only where the settings or the
    /// gems moved, and scanning a file again only where it changed.
    fn main_locale_files(&self, reading: &Reading<'_>) -> Vec<PathBuf> {
        let mut walk = self
            .walked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let key = (
            reading.root.to_path_buf(),
            reading.gems.iter().map(|gem| gem.path.clone()).collect(),
            reading.i18n.locale.clone(),
            reading.i18n.paths.clone(),
        );
        if walk.key.as_ref() != Some(&key) {
            walk.files = Self::candidates(reading);
            walk.key = Some(key);
        }
        let locale = &reading.i18n.locale;
        let Walk { files, locales, .. } = &mut *walk;
        files
            .iter()
            .filter(|path| {
                let stamp = stamp_of(path);
                let held = locales.get(*path).filter(|(held, _)| *held == stamp);
                let written = match held {
                    Some((_, written)) => written.clone(),
                    None => {
                        let ruby = path.extension().is_some_and(|x| x == "rb");
                        let written = std::fs::read_to_string(path)
                            .map(|source| {
                                if ruby {
                                    i18n::ruby_locales_in(&source)
                                } else {
                                    i18n::locales_in(&source)
                                }
                            })
                            .unwrap_or_default();
                        locales.insert((*path).clone(), (stamp, written.clone()));
                        written
                    }
                };
                written.contains(locale)
            })
            .cloned()
            .collect()
    }

    /// Forget the walk, so the next discovery walks again.
    fn forget_walk(&self) {
        let mut walk = self
            .walked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        walk.key = None;
    }

    /// The call as [`Translations::returns`] reads it, where the member looks a key up and the
    /// call says which for certain: a literal key that is not relative, a literal `scope:`, and
    /// no `locale:` other than the main one.
    fn asked(&self, keyed: &Keyed<'_>, locale: &str) -> Option<Asked> {
        let (html_safe, _) = i18n::lookup(keyed.member)?;
        if keyed.key.is_empty() || keyed.key.starts_with('.') {
            return None;
        }
        let mut asked = Asked {
            key: keyed.key.to_owned(),
            scope: Vec::new(),
            count: false,
            other_default: false,
            html_safe,
        };
        for (name, written) in keyed.keywords {
            match (name.as_str(), written) {
                ("scope", Written::Text(scope) | Written::Symbol(scope)) => {
                    asked.scope = scope.split('.').map(str::to_owned).collect();
                }
                ("scope", Written::Other) => return None,
                ("count", _) => asked.count = true,
                ("default", Written::Text(_)) => {}
                ("default", _) => asked.other_default = true,
                ("locale", Written::Text(named) | Written::Symbol(named)) if named == locale => {}
                ("locale", _) => return None,
                _ => {}
            }
        }
        Some(asked)
    }

    /// The main locale, as the last walk read it.
    fn locale(&self) -> String {
        let walk = self
            .walked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        walk.key
            .as_ref()
            .map_or_else(|| "en".to_owned(), |(_, _, locale, _)| locale.clone())
    }
}

/// A file's modification time and length, or `None` where it is gone.
fn stamp_of(path: &Path) -> Stamp {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.modified().ok()?, metadata.len()))
}

/// Whether a path is a locale file i18n loads: a `.yml` or a `.rb`.
fn is_locale_file(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension == "yml" || extension == "rb")
}

/// The locale files in a directory, sorted, and in its subdirectories where `deep`.
fn files_in(directory: &Path, deep: bool) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(directory) else {
        return files;
    };
    let mut entries: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if deep {
                files.extend(files_in(&path, true));
            }
        } else if is_locale_file(&path) {
            files.push(path);
        }
    }
    files
}

/// Every `config/locales` directory under `directory`, skipping the trees no application loads a
/// locale from, and not deeper than the depth an engine or a plugin sits at.
fn locale_directories(directory: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    let locales = directory.join("config").join("locales");
    if locales.is_dir() {
        found.push(locales);
    }
    if depth >= 3 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    !name.starts_with('.') && !SKIPPED.contains(&name) && name != "config"
                })
        })
        .collect();
    children.sort();
    for child in children {
        locale_directories(&child, depth + 1, found);
    }
}

/// What a card or a completion item shows for what a key holds.
fn shown(entry: &Entry) -> String {
    match &entry.held {
        Held::Text(text) => text.clone(),
        Held::Tree if entry.plural() => "a plural: one of several texts, by `count:`".to_owned(),
        Held::Tree => format!("a subtree of {} keys", entry.children.len()),
        Held::List { .. } => "a list".to_owned(),
        Held::Other => "a value that is not a string".to_owned(),
    }
}

/// The lists this module reads: none. Its files are not graph documents; they are found by
/// [`super::Knowledge::discover`].
static WANTS: [Wants; 0] = [];

/// The projection the main-locale files ride on, so the pass stamps them and a changed one is
/// read again ([`super::Projects::also_reads`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Projection {
    files: Vec<DocUri>,
}

impl super::Projects for Projection {
    fn absorb(&mut self, _uri: &str, _contribution: &dyn super::Contributes) {}

    fn declares_nothing(&self) -> bool {
        self.files.is_empty()
    }

    fn also_reads(&self) -> &[DocUri] {
        &self.files
    }

    fn same_as(&self, other: &dyn super::Projects) -> bool {
        other.as_any().downcast_ref::<Self>() == Some(self)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

impl super::Knowledge for Translate {
    fn name(&self) -> &'static str {
        "i18n"
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn wants(&self) -> &'static [Wants] {
        &WANTS
    }

    fn wanted(&self, _list: ListId, _features: Features) -> bool {
        false
    }

    fn spellable_names(&self) -> Vec<&'static str> {
        LOOKUPS
            .iter()
            .map(|(owner, _, _, _)| *owner)
            .chain(RETURNS.iter().map(|(owner, _, _, _)| *owner))
            .chain(i18n::NAMESPACES)
            .collect()
    }

    fn projection(&self) -> Option<Box<dyn super::Projects>> {
        Some(Box::new(Projection::default()))
    }

    fn discover(&self, reading: &Reading<'_>) -> Vec<DocUri> {
        if !reading.features.i18n {
            return Vec::new();
        }
        self.main_locale_files(reading)
            .iter()
            .filter_map(|path| DocUri::from_path(path))
            .collect()
    }

    fn discovered(&mut self, found: Vec<DocUri>) {
        self.found = found;
    }

    fn touched(&mut self, path: &Path) -> bool {
        let under_locales = path
            .components()
            .collect::<Vec<_>>()
            .windows(2)
            .any(|pair| pair[0].as_os_str() == "config" && pair[1].as_os_str() == "locales");
        let listed = self
            .found
            .iter()
            .any(|uri| uri.to_file_path().as_deref() == Some(path));
        if under_locales || listed {
            self.forget_walk();
        }
        (under_locales && is_locale_file(path)) || listed
    }

    fn after_the_walk(&self, context: &mut super::Context) {
        let mut files = self.found.clone();
        files.sort_unstable();
        if let Some(projection) = context.projection_mut::<Projection>() {
            projection.files = files;
        }
    }

    /// Read every main-locale file whose text moved, and merge them all again in load order where
    /// any did. The load order is the one discovery walked, which `found` keeps.
    fn refresh(&mut self, sources: &Sources<'_>) {
        if !sources.features.i18n {
            self.read.clear();
            self.table = Arc::default();
            return;
        }
        let locale = self.locale();
        let order: Vec<DocUri> = {
            let walk = self
                .walked
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            walk.files
                .iter()
                .filter_map(|path| DocUri::from_path(path))
                .filter(|uri| self.found.contains(uri))
                .collect()
        };
        let before = self.read.len();
        self.read
            .retain(|key, _| order.iter().any(|uri| uri.as_str() == key));
        let mut moved = before != self.read.len();
        for (index, uri) in order.iter().enumerate() {
            let fresh = (sources.fresh)(uri);
            if self
                .read
                .get(uri.as_str())
                .is_some_and(|(held, _)| *held == fresh)
            {
                continue;
            }
            moved = true;
            let Some(text) = (sources.text)(uri) else {
                self.read.remove(uri.as_str());
                continue;
            };
            self.reads += 1;
            let keys = if uri.as_str().ends_with(".rb") {
                i18n::read_ruby(&text, &locale, index)
            } else {
                i18n::read_locale(&text, &locale, index)
            };
            self.read
                .insert(uri.as_str().to_owned(), (fresh, keys.unwrap_or_default()));
        }
        if !moved && !self.table.files.is_empty() {
            return;
        }
        let mut table = Translations {
            files: order.iter().map(|uri| uri.as_str().to_owned()).collect(),
            ..Translations::default()
        };
        for (index, uri) in order.iter().enumerate() {
            if let Some((_, keys)) = self.read.get(uri.as_str()) {
                // Each entry was read with its file's index in `order` at the time; the order is
                // rebuilt with every walk, so the index is set again here.
                table.merge(reindexed(keys.clone(), index));
            }
        }
        self.table = Arc::new(table);
    }

    /// Which members look a key up, and i18n's own returns, on the documents declaring each owner.
    fn declare(&mut self, declaring: &Declaring<'_>, into: &mut Declared) -> Counted {
        if !declaring.features.i18n {
            return Counted::new();
        }
        let namespaces = &declaring.context.namespaces;
        let owners: std::collections::BTreeSet<String> = LOOKUPS
            .iter()
            .map(|(owner, _, _, _)| (*owner).to_owned())
            .chain(RETURNS.iter().map(|(owner, _, _, _)| (*owner).to_owned()))
            .collect();
        let hosts = (declaring.declares)(&owners);
        let mut declared = 0;
        let mut facts: BTreeMap<&str, (&DocUri, Facts)> = BTreeMap::new();
        let owned = |owner: &str| {
            if namespaces.opens(owner) {
                Owner::Module(owner.to_owned())
            } else {
                Owner::Instance(owner.to_owned())
            }
        };
        let rows = LOOKUPS
            .iter()
            .map(|(owner, name, _, _)| -> Returned {
                (
                    owner,
                    name,
                    (i18n::lookup_parameters(owner, name), KEYED),
                    &[],
                )
            })
            .chain(RETURNS.iter().copied());
        for (owner, name, (parameters, returns), overloads) in rows {
            let Some(uri) = hosts.get(owner).filter(|_| namespaces.spellable(owner)) else {
                continue;
            };
            declared += 1;
            let (_, held) = facts
                .entry(owner)
                .or_insert_with(|| (uri, Facts::default()));
            held.declare(Row {
                owner: owned(owner),
                name: name.to_owned(),
                returns: returns.to_owned(),
                parameters: parameters.to_owned(),
                because: format!(
                    "What i18n's `{name}` hands back, which its body does not say to a reader; \
                     the method itself is declared in the bundle."
                ),
                at: None,
                from: Source::Interface,
                overloads: overloads
                    .iter()
                    .map(|(parameters, returns)| ((*parameters).to_owned(), (*returns).to_owned()))
                    .collect(),
                private: false,
            });
        }
        for (uri, facts) in facts.into_values() {
            super::add(into, uri, facts);
        }
        vec![
            ("i18n rows", declared),
            ("translation files", self.table.files.len()),
        ]
    }

    fn keyed_type(&self, keyed: &Keyed<'_>) -> Option<&'static str> {
        let (_, yields) = i18n::lookup(keyed.member)?;
        if yields && keyed.block {
            return None;
        }
        let asked = self.asked(keyed, &self.locale())?;
        self.table.returns(&asked)
    }

    fn keyed_entry(&self, keyed: &Keyed<'_>) -> Option<KeyedEntry> {
        let asked = self.asked(keyed, &self.locale())?;
        let entry = self.table.get(&asked.path())?;
        Some(KeyedEntry {
            uri: self.table.files.get(entry.file)?.clone(),
            at: entry.at,
            shown: i18n::yaml(keyed.key, entry),
        })
    }

    fn keyed_under(&self, member: &str, prefix: &str) -> Option<Vec<KeyedChild>> {
        i18n::lookup(member)?;
        let under = self.table.under(prefix)?;
        Some(
            under
                .iter()
                .map(|(name, entry)| KeyedChild {
                    name: name.clone(),
                    branch: entry.held == Held::Tree,
                    shown: shown(entry),
                })
                .collect(),
        )
    }
}

/// A file's keys with `file` as their file's index, all the way down.
fn reindexed(keys: BTreeMap<String, Entry>, file: usize) -> BTreeMap<String, Entry> {
    keys.into_iter()
        .map(|(name, entry)| {
            (
                name,
                Entry {
                    file,
                    children: reindexed(entry.children, file),
                    ..entry
                },
            )
        })
        .collect()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::knowledge::{Context, Knowledge};
    use crate::workspace::Gem;
    use crate::workspace::config::I18nConfig;

    fn features(i18n: bool) -> Features {
        Features {
            rails: false,
            schema: false,
            models: false,
            routes: false,
            entrypoints: false,
            views: false,
            structs: false,
            annotations: false,
            rspec: false,
            factories: false,
            i18n,
        }
    }

    fn write(root: &Path, relative: &str, text: &str) -> PathBuf {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        path
    }

    fn gem(root: &Path, name: &str) -> Gem {
        Gem {
            name: name.to_owned(),
            full_name: format!("{name}-1.0"),
            path: root.join(name),
            load_paths: Vec::new(),
        }
    }

    /// A project with an engine, a test tree and a `node_modules`, two gems (Rails' own library and
    /// an engine gem), and a gem whose `lib/` holds a locale nobody loads.
    fn project() -> (tempfile::TempDir, Vec<Gem>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "app/config/locales/en.yml",
            "en:\n  app: own\n  shared: app\n",
        );
        write(root, "app/config/locales/de.yml", "de:\n  app: eigen\n");
        write(
            root,
            "app/config/locales/en.rb",
            "{ en: { ruby: \"x\" } }\n",
        );
        write(
            root,
            "app/config/locales/nested/admin.en.yml",
            "en:\n  admin:\n    title: Admin\n",
        );
        write(root, "app/config/locales/notes.txt", "en: no\n");
        write(
            root,
            "app/engines/shop/config/locales/en.yml",
            "en:\n  shared: engine\n  shop: Shop\n",
        );
        write(
            root,
            "app/spec/dummy/config/locales/en.yml",
            "en:\n  dummy: x\n",
        );
        write(
            root,
            "app/node_modules/x/config/locales/en.yml",
            "en:\n  npm: x\n",
        );
        write(
            root,
            "app/a/b/c/d/config/locales/en.yml",
            "en:\n  deep: x\n",
        );
        write(root, "app/listed/extra.en.yml", "en:\n  listed: x\n");
        write(
            root,
            "app/.hidden/config/locales/en.yml",
            "en:\n  hidden: x\n",
        );
        let gems = root.join("gems");
        write(
            &gems,
            "activesupport/lib/active_support/locale/en.yml",
            "en:\n  shared: rails\n  number: x\n",
        );
        write(
            &gems,
            "devise/config/locales/en.yml",
            "en:\n  shared: devise\n  devise: x\n",
        );
        write(&gems, "other/lib/other/locale/en.yml", "en:\n  other: x\n");
        // Rails' own directory is read flat: i18n's railtie loads `locale/*.yml` alone.
        write(
            &gems,
            "activesupport/lib/active_support/locale/sub/deep.yml",
            "en:\n  deep: x\n",
        );
        let gems = vec![
            gem(&gems, "activesupport"),
            gem(&gems, "devise"),
            gem(&gems, "other"),
        ];
        (dir, gems)
    }

    fn found(
        translate: &Translate,
        root: &Path,
        gems: &[Gem],
        i18n: &I18nConfig,
        rb: bool,
    ) -> Vec<String> {
        let admits = move |path: &Path| rb || path.extension().is_none_or(|x| x != "rb");
        let reading = Reading {
            root: &root.join("app"),
            admits: &admits,
            features: features(true),
            gems,
            i18n,
        };
        translate
            .discover(&reading)
            .iter()
            .map(|uri| {
                let path = uri.to_file_path().unwrap();
                path.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    /// i18n's load path: Rails' own files, then every gem's `config/locales`, then the project's,
    /// the application's own last. Only the main locale's files, and never a test tree's.
    #[test]
    fn discovery_walks_the_load_path_in_i18n_s_order() {
        let (dir, gems) = project();
        let translate = Translate::default();
        let i18n = I18nConfig::default();
        assert_eq!(
            found(&translate, dir.path(), &gems, &i18n, true),
            [
                "gems/activesupport/lib/active_support/locale/en.yml",
                "gems/devise/config/locales/en.yml",
                "app/engines/shop/config/locales/en.yml",
                "app/config/locales/en.rb",
                "app/config/locales/en.yml",
                "app/config/locales/nested/admin.en.yml",
            ]
        );
        // A `.rb` the project's `index` settings leave out is not read.
        let translate = Translate::default();
        assert!(
            !found(&translate, dir.path(), &gems, &i18n, false)
                .contains(&"app/config/locales/en.rb".to_owned())
        );
        // Another main locale reads its own files.
        let german = I18nConfig {
            locale: "de".to_owned(),
            ..I18nConfig::default()
        };
        assert_eq!(
            found(&translate, dir.path(), &gems, &german, true),
            ["app/config/locales/de.yml"]
        );
        // A root that is not there holds nothing of its own.
        let gone = Reading {
            root: &dir.path().join("gone"),
            admits: &|_| true,
            features: features(true),
            gems: &[],
            i18n: &i18n,
        };
        assert!(Translate::default().discover(&gone).is_empty());
        let mut nothing = Translate::default();
        refresh(&mut nothing, true);
        assert_eq!(type_of(&nothing, "app"), None);
        // Off, nothing is found.
        let reading = Reading {
            root: dir.path(),
            admits: &|_| true,
            features: features(false),
            gems: &gems,
            i18n: &i18n,
        };
        assert!(translate.discover(&reading).is_empty());
    }

    /// `[i18n] paths` replaces the project's own list, and `[]` reads none of it; the gems' files
    /// are read either way.
    #[test]
    fn a_path_list_replaces_the_project_s_own_files() {
        let (dir, gems) = project();
        let listed = I18nConfig {
            paths: Some(vec!["listed/*.yml".to_owned(), "listed/*.txt".to_owned()]),
            ..I18nConfig::default()
        };
        assert_eq!(
            found(&Translate::default(), dir.path(), &gems, &listed, true),
            [
                "gems/activesupport/lib/active_support/locale/en.yml",
                "gems/devise/config/locales/en.yml",
                "app/listed/extra.en.yml",
            ]
        );
        let none = I18nConfig {
            paths: Some(Vec::new()),
            ..I18nConfig::default()
        };
        assert_eq!(
            found(&Translate::default(), dir.path(), &gems, &none, true).len(),
            2
        );
    }

    /// Reading from disk the way core's closures do.
    fn refresh(translate: &mut Translate, on: bool) {
        let context = Context::default();
        let fresh = |uri: &DocUri| Fresh::Disk(stamp_of(&uri.to_file_path().unwrap()));
        let text = |uri: &DocUri| std::fs::read_to_string(uri.to_file_path().unwrap()).ok();
        let sources = Sources {
            context: &context,
            features: features(on),
            fresh: &fresh,
            held: &|_| false,
            text: &text,
            caption: &|uri| uri.as_str().to_owned(),
        };
        translate.refresh(&sources);
    }

    fn keyed<'a>(member: &'a str, key: &'a str, keywords: &'a [(String, Written)]) -> Keyed<'a> {
        Keyed {
            member,
            key,
            keywords,
            block: false,
        }
    }

    fn type_of(translate: &Translate, key: &str) -> Option<&'static str> {
        translate.keyed_type(&keyed("I18n::Base#t()", key, &[]))
    }

    /// Later files win, a file is parsed again only when it changed, and a file that is gone is
    /// forgotten.
    #[test]
    fn later_files_win_and_an_unchanged_file_is_not_read_again() {
        let (dir, gems) = project();
        let mut translate = Translate::default();
        let i18n = I18nConfig::default();
        let reading = Reading {
            root: &dir.path().join("app"),
            admits: &|_| true,
            features: features(true),
            gems: &gems,
            i18n: &i18n,
        };
        let uris = translate.discover(&reading);
        translate.discovered(uris);
        refresh(&mut translate, true);
        assert_eq!(translate.reads, 6);
        let shared = translate
            .keyed_entry(&keyed("I18n::Base#t()", "shared", &[]))
            .unwrap();
        assert_eq!(shared.shown, "shared: app");
        assert!(
            shared.uri.ends_with("app/config/locales/en.yml"),
            "{}",
            shared.uri
        );
        assert_eq!(shared.at, (17, 23));
        assert_eq!(type_of(&translate, "ruby"), Some("String"));
        assert_eq!(type_of(&translate, "admin.title"), Some("String"));
        assert_eq!(type_of(&translate, "dummy"), None);
        refresh(&mut translate, true);
        assert_eq!(translate.reads, 6);
        // A changed file is read again: its length moved.
        write(
            dir.path(),
            "app/config/locales/en.yml",
            "en:\n  app: own\n  shared:\n    now: a tree\n",
        );
        refresh(&mut translate, true);
        assert_eq!(translate.reads, 7);
        assert_eq!(type_of(&translate, "shared"), Some("Hash[Symbol, untyped]"));
        // A file gone from disk is dropped, and the key falls to the file below it.
        std::fs::remove_file(dir.path().join("app/config/locales/en.yml")).unwrap();
        refresh(&mut translate, true);
        assert_eq!(type_of(&translate, "shared"), Some("String"));
        assert_eq!(
            translate
                .keyed_entry(&keyed("I18n::Base#t()", "shared", &[]))
                .unwrap()
                .shown,
            "shared: engine"
        );
        // Discovery that no longer lists a file forgets what it read there.
        translate.discovered(Vec::new());
        refresh(&mut translate, true);
        assert_eq!(type_of(&translate, "shop"), None);
        // Turned off, nothing is held.
        translate.discovered(translate.discover(&reading));
        refresh(&mut translate, true);
        assert_eq!(type_of(&translate, "shop"), Some("String"));
        refresh(&mut translate, false);
        assert_eq!(type_of(&translate, "shop"), None);
    }

    /// A watched change under `config/locales` walks again; a file discovery listed is read again
    /// wherever it is; anything else is not this module's.
    #[test]
    fn a_change_under_a_locales_directory_is_this_module_s() {
        let (dir, gems) = project();
        let mut translate = Translate::default();
        let i18n = I18nConfig::default();
        let reading = Reading {
            root: &dir.path().join("app"),
            admits: &|_| true,
            features: features(true),
            gems: &gems,
            i18n: &i18n,
        };
        let uris = translate.discover(&reading);
        translate.discovered(uris);
        let root = dir.path();
        assert!(translate.touched(&root.join("app/config/locales/fr.yml")));
        assert!(
            translate.touched(&root.join("gems/activesupport/lib/active_support/locale/en.yml"))
        );
        assert!(!translate.touched(&root.join("app/config/locales/notes.txt")));
        assert!(!translate.touched(&root.join("app/models/user.rb")));
        // A new file in a `config/locales` directory is found by the next walk.
        write(root, "app/config/locales/more.en.yml", "en:\n  more: x\n");
        assert!(translate.touched(&root.join("app/config/locales/more.en.yml")));
        let uris = translate.discover(&reading);
        assert_eq!(uris.len(), 7);
        translate.discovered(uris);
        let mut context = Context::default();
        translate.after_the_walk(&mut context);
    }

    /// Which calls say for certain which key they look up.
    #[test]
    fn a_call_names_its_key_only_where_the_text_says_so() {
        let translate = Translate::default();
        let asked = |member: &str, key: &str, keywords: &[(String, Written)]| {
            translate.asked(&keyed(member, key, keywords), "en")
        };
        let text = |name: &str, value: &str| (name.to_owned(), Written::Text(value.to_owned()));
        let symbol = |name: &str, value: &str| (name.to_owned(), Written::Symbol(value.to_owned()));
        let other = |name: &str| (name.to_owned(), Written::Other);
        assert!(asked("Kernel#puts()", "x", &[]).is_none());
        assert!(asked("I18n", "x", &[]).is_none());
        assert!(asked("I18n::Base#t()", "", &[]).is_none());
        assert!(asked("I18n::Base#t()", ".relative", &[]).is_none());
        let plain = asked("I18n::Base#t()", "a.b", &[]).unwrap();
        assert_eq!(plain.path(), "a.b");
        assert!(!plain.html_safe);
        assert!(
            asked("AbstractController::Translation#t()", "a", &[])
                .unwrap()
                .html_safe
        );
        let scoped = asked("I18n::Base#t()", "c", &[symbol("scope", "a.b")]).unwrap();
        assert_eq!(scoped.path(), "a.b.c");
        assert_eq!(
            asked("I18n::Base#t()", "c", &[text("scope", "a")])
                .unwrap()
                .path(),
            "a.c"
        );
        assert!(asked("I18n::Base#t()", "c", &[other("scope")]).is_none());
        assert!(
            asked("I18n::Base#t()", "c", &[other("count")])
                .unwrap()
                .count
        );
        let defaulted = asked("I18n::Base#t()", "c", &[text("default", "x")]).unwrap();
        assert!(!defaulted.other_default);
        assert!(
            asked("I18n::Base#t()", "c", &[symbol("default", "x")])
                .unwrap()
                .other_default
        );
        assert!(asked("I18n::Base#t()", "c", &[symbol("locale", "en")]).is_some());
        assert!(asked("I18n::Base#t()", "c", &[text("locale", "en")]).is_some());
        assert!(asked("I18n::Base#t()", "c", &[symbol("locale", "de")]).is_none());
        assert!(asked("I18n::Base#t()", "c", &[other("locale")]).is_none());
        assert!(asked("I18n::Base#t()", "c", &[other("raise")]).is_some());
    }

    /// Completion's keys, and the view's `t` with a block.
    #[test]
    fn the_keys_under_a_prefix_are_the_main_locale_s() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "config/locales/en.yml",
            "en:\n  top:\n    leaf: Leaf\n    items:\n      one: one\n      other: many\n    \
             tree:\n      a: x\n    days: [Mon]\n    count: 1\n",
        );
        let mut translate = Translate::default();
        let i18n = I18nConfig::default();
        let reading = Reading {
            root: dir.path(),
            admits: &|_| true,
            features: features(true),
            gems: &[],
            i18n: &i18n,
        };
        let uris = translate.discover(&reading);
        translate.discovered(uris);
        refresh(&mut translate, true);
        assert!(translate.keyed_under("Kernel#puts()", "top").is_none());
        assert!(translate.keyed_under("Kernel", "top").is_none());
        assert!(translate.keyed_under("I18n::Base#t()", "nowhere").is_none());
        let under = translate.keyed_under("I18n::Base#t()", "top").unwrap();
        let shown: Vec<(&str, bool, &str)> = under
            .iter()
            .map(|child| (child.name.as_str(), child.branch, child.shown.as_str()))
            .collect();
        assert_eq!(
            shown,
            [
                ("count", false, "a value that is not a string"),
                ("days", false, "a list"),
                ("items", true, "a plural: one of several texts, by `count:`"),
                ("leaf", false, "Leaf"),
                ("tree", true, "a subtree of 1 keys"),
            ]
        );
        let block = |member| {
            translate.keyed_type(&Keyed {
                block: true,
                ..keyed(member, "top.leaf", &[])
            })
        };
        assert_eq!(block("I18n::Base#t()"), Some("String"));
        assert_eq!(block("ActionView::Helpers::TranslationHelper#t()"), None);
        assert_eq!(block("Kernel"), None);
        assert!(
            translate
                .keyed_entry(&keyed("I18n::Base#t()", "nowhere", &[]))
                .is_none()
        );
        assert!(
            translate
                .keyed_entry(&keyed("Kernel#puts()", "top", &[]))
                .is_none()
        );
    }

    /// The projection compares by the files it holds.
    #[test]
    fn the_projection_is_its_files() {
        use super::super::Projects;
        let mut one = Projection::default();
        assert!(one.declares_nothing());
        one.as_any_mut()
            .downcast_mut::<Projection>()
            .unwrap()
            .files
            .push(DocUri::from_path(Path::new("/x/en.yml")).unwrap());
        assert!(!one.declares_nothing());
        assert_eq!(one.also_reads().len(), 1);
        assert!(one.same_as(&one.clone()));
        assert!(!one.same_as(&Projection::default()));
        assert_eq!(Translate::default().name(), "i18n");
        assert!(Translate::default().as_any().is::<Translate>());
        assert!(Translate::default().wants().is_empty());
        assert!(!Translate::default().wanted(ListId("rails.models"), features(true)));
        assert!(
            Translate::default()
                .spellable_names()
                .contains(&"ActionView::Helpers")
        );
        assert!(Translate::default().projection().is_some());
    }
}
