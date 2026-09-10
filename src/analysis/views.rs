//! What a template can call, and where a bare word in one comes from.
//!
//! Every other answer in this crate starts from something the file under the cursor writes down:
//! a receiver, a constant, a `def`. A template writes none of them. `<%= time_ago(story) %>` is a
//! call on an **implicit receiver** whose class Rails builds at render time out of two things no
//! file names — every module under `app/helpers`, and the `helper_method` proxies of the
//! controller the template's *path* implies — so rubydex correctly resolves it to nothing.
//!
//! **A generated `module` holding the view context cannot work**, which is why this is a rung
//! and not a declaration. RBS has no way to say *self in this file is X*, so the module would
//! exist and nothing would reach it; the obvious repair of wrapping the template's Ruby in a
//! `class … end` is closed by construction, because [`erb::ruby_view`] replaces markup with one
//! space per byte so every offset rubydex records is the template's own. Declaring the helpers
//! half in RBS would also give a second *place* to every helper method in the project.
//!
//! So it is one table, consulted by [`locator::resolve_typed`] where every other rung is and by
//! `completion` at the same cursor. The two read one walk, exactly as the concern edge reads
//! [`locator::extended_class_methods`]: resolution takes the first answer, completion collects
//! all of them, and the gate deciding what the view context *is* is stated once, here.
//!
//! # The two halves
//!
//! **`app/helpers`** needs no macro read. Rails globs `**/*_helper.rb` under each `app/helpers`
//! directory and includes every module it finds, so the `def`s are already in the graph and what
//! is missing is only the edge — [`rails::is_helper`] and a list of names. This is the large half
//! by a wide margin.
//!
//! **`helper_method`** is the machinery. `AbstractController::Helpers` writes `def current_user`
//! per call onto the controller's `_helpers` module, so what the macro hands over is a
//! *permission* rather than a member: the `def` it names is already on the controller.
//!
//! # What the table refuses
//!
//! - **A name that is neither.** The view context is the two halves *plus* every helper module
//!   ActionView ships, which this crate does not model, so the rung is additive: a template
//!   calling `link_to` still answers on the name rung. An application's own `def tag` in
//!   `ApplicationHelper`, written to shadow `ActionView::Helpers::TagHelper#tag`, is what a
//!   template's `tag` resolves to instead of a long candidate list.
//! - **A controller method nobody exported.** `helper_method` is the whole of the gate on that
//!   half, per name and never per class: a template may call `current_user` because the class
//!   said so, and may not call `set_story` because it did not.
//! - **A mailer's views do not get the helpers half.** `include_all_helpers` is
//!   `ActionController::Base`'s default and `ActionMailer::Base` has no such thing — a mailer
//!   reaches an application helper only by writing `helper` itself, which this reader does not
//!   read. A mailer template gets its own exports and nothing else.
//!
//! Two bounds are stated rather than discovered. **Partials**: `rails::controller_of` reads the
//! directory and not the file name, so `shared/_header.html.erb` names a `SharedController`
//! nothing defines, and a partial under such a directory gets the helpers half only.
//! **`include_all_helpers = false`**: an application that sets it gets only its matching helper,
//! and that config is out of reach for the same reason `database.yml` is.

use std::collections::{BTreeMap, BTreeSet};

use rubydex::model::{
    declaration::{Ancestor, Declaration, Namespace},
    graph::Graph,
    ids::{DeclarationId, StringId, UriId},
};
use rubydex::query;

use super::erb;
use super::types::declared;
use crate::workspace::{DocUri, rails};

/// How a bare name in a template was reached, for the card to say.
///
/// Both are conventions rather than facts a file states, which is what puts this answer in the
/// *derived* tier beside the view↔controller rung it sits next to: nothing in the template says
/// which class renders it, and nothing in it says that `app/helpers` is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InView {
    /// A `def` in a module under `app/helpers`, which Rails includes in every view context.
    Helper,
    /// A `helper_method` written by the class the template's path names, or by one of its
    /// ancestors. The name is the class Rails renders the template from.
    Exported(String),
}

/// One member a bare word in a template reaches.
pub struct Reached {
    pub declaration: DeclarationId,
    /// How far out the module that installed it sits, on the chain Rails builds `_helpers` from.
    ///
    /// A `helper_method` proxy is written **on** `_helpers` and a helper module is `include`d
    /// into it, so an export is nearer than a helper by exactly one step — which is also the
    /// order the two halves answer in when both hold the name, and Ruby's own answer for a
    /// module that defines a method its includer also defines.
    pub step: u16,
}

/// What a bare name is reached through, and the declaration it names.
pub struct Found {
    pub declaration: DeclarationId,
    pub how: InView,
}

/// Which modules and which exports a template can see, rebuilt on every settle.
///
/// Held beside the graph for [`super::synthesized::Synthesized`]'s reason and filled by the same
/// pass: both halves are projections of what the generators already walked, and neither is a
/// declaration. `Default` is a workspace with no Rails in it, which answers nothing and costs a
/// path test per hover to say so.
#[derive(Debug, Default)]
pub struct Views {
    /// Every `app/helpers/**/*_helper.rb` module the user's own code defines, fully spelled and
    /// sorted, so that two helper modules spelling one name answer the same way on every run.
    helpers: Vec<String>,
    /// A body that wrote a `helper_method`, and the names it handed over.
    ///
    /// Keyed by the class or module the macro is written in rather than by the controller it
    /// ends up reaching, because a concern does not know its includers and does not have to:
    /// the ancestor walk below crosses the `include` the controller already wrote.
    exports: BTreeMap<String, BTreeSet<String>>,
    /// A body that wrote a `helper`, and the modules it put into its own view context.
    ///
    /// The other half of the mailer story, and the reason the mailer gate below is a bound
    /// rather than a wall. `ActionMailer::Base` has no `include_all_helpers`, so a mailer
    /// reaches an application helper only by naming it — and 20 of the six corpora's 26
    /// `helper` calls are in a mailer, 15 of them mastodon's, which is 22 of the 25 template
    /// sites that corpus has. A controller writing one is usually saying nothing new; the six
    /// that do name a module a gem ships, which the `app/helpers` glob does not reach either.
    included: BTreeMap<String, BTreeSet<String>>,
    /// The classes the application defines that a **mailer's** view directory may name.
    ///
    /// A second gate and not a widening of the first: `rails::controller_of` produces a name
    /// nothing but a controller is called, and `rails::mailer_of` produces whatever the
    /// directory happens to spell — `app/views/shared/` spells `Shared` — so the second is only
    /// ever consulted against this list.
    mailers: BTreeSet<String>,
}

impl Views {
    #[must_use]
    pub fn new(
        helpers: Vec<String>,
        exports: BTreeMap<String, BTreeSet<String>>,
        included: BTreeMap<String, BTreeSet<String>>,
        mailers: BTreeSet<String>,
    ) -> Self {
        Self {
            helpers,
            exports,
            included,
            mailers,
        }
    }

    /// What the template filed under `uri_id` can call, or nothing.
    ///
    /// `None` for every document that is not a template, which is the first test and the cheap
    /// one: this is asked of every call in the project that rubydex could not resolve, and a
    /// `.rb` file must pay a path test and no more. `None` also where the two halves are both
    /// empty, so that a Rails application with no helpers and no exports costs nothing further.
    #[must_use]
    pub fn reachable(&self, graph: &Graph, uri_id: UriId) -> Option<Reachable> {
        let path = DocUri::from_uri_str(graph.documents().get(&uri_id)?.uri())?.to_path()?;
        if !erb::is_template(&path) {
            return None;
        }

        // The controller first and the mailer only where there is no controller, which is the
        // order Rails resolves them in and not a preference: `app/views/user_mailer/` names a
        // `UserMailerController` that does not exist, and an application that defines one has
        // said something this rule must not overrule.
        let renderer = rails::controller_of(&path)
            .and_then(|name| Some((name.clone(), declared(graph, &name)?, true)))
            .or_else(|| {
                let name = rails::mailer_of(&path)?;
                self.mailers
                    .contains(&name)
                    .then(|| Some((name.clone(), declared(graph, &name)?, false)))
                    .flatten()
            });

        let named = renderer
            .as_ref()
            .map(|(_, declaration, _)| self.named_by(graph, *declaration))
            .unwrap_or_default();
        let renderer = renderer.map(|(name, declaration, controller)| Renderer {
            exported: self.exported_by(graph, declaration),
            name,
            declaration,
            controller,
        });
        // A mailer's own views are the one place the glob does not apply — see the module docs
        // — and a template whose class does not exist at all still gets it, which is what a
        // partial under `shared/` lives on. What a mailer gets instead is what it asked for by
        // name, which is the whole of `helper`'s reason for existing.
        let globbed: &[String] = if renderer.as_ref().is_none_or(|renderer| renderer.controller) {
            &self.helpers
        } else {
            &[]
        };
        let helpers = deduplicated(
            globbed
                .iter()
                .chain(named.iter())
                .filter_map(|name| declared(graph, name))
                .collect(),
        );

        let reachable = Reachable { renderer, helpers };
        (!reachable.is_empty()).then_some(reachable)
    }

    /// Every module `declaration` or one of its ancestors named with `helper`.
    ///
    /// The ancestor walk is [`Views::exported_by`]'s and is load-bearing in the same way:
    /// mastodon writes `helper :application` once, in `ApplicationMailer`, and means it for
    /// every mailer under it.
    fn named_by(&self, graph: &Graph, declaration: DeclarationId) -> BTreeSet<String> {
        let mut named: BTreeSet<String> = BTreeSet::new();
        for (name, _) in ancestors_of(graph, declaration) {
            if let Some(modules) = self.included.get(name) {
                named.extend(modules.iter().cloned());
            }
        }
        named
    }

    /// Every name `declaration` or one of its ancestors handed to the view context.
    ///
    /// The ancestor walk is the whole of what makes three of the macro's four hosts work
    /// without a case for any of them: a `helper_method` in a concern, in a module under
    /// `app/helpers` that the controller `include`s, and in `ApplicationController` are all one
    /// question — is the class this template's path names below the body that wrote the macro —
    /// and rubydex answered it at index time.
    fn exported_by(&self, graph: &Graph, declaration: DeclarationId) -> BTreeSet<String> {
        let mut exported: BTreeSet<String> = BTreeSet::new();
        for (name, _) in ancestors_of(graph, declaration) {
            if let Some(names) = self.exports.get(name) {
                exported.extend(names.iter().cloned());
            }
        }
        exported
    }
}

/// The class a template's path names, and what it lets a template call.
struct Renderer {
    name: String,
    declaration: DeclarationId,
    /// Whether it is a controller rather than a mailer, which is the one thing the two spellings
    /// decide differently.
    controller: bool,
    exported: BTreeSet<String>,
}

/// One template's view context, resolved against the graph.
pub struct Reachable {
    renderer: Option<Renderer>,
    helpers: Vec<DeclarationId>,
}

impl Reachable {
    fn is_empty(&self) -> bool {
        self.helpers.is_empty()
            && self
                .renderer
                .as_ref()
                .is_none_or(|renderer| renderer.exported.is_empty())
    }

    /// The declaration a bare `member` written in this template names.
    ///
    /// `member` is rubydex's parenthesised spelling, because that is what the caller already
    /// has and what the lookup needs; the export list holds the symbols the macro was written
    /// with, so the parentheses come off for that test and only for it.
    ///
    /// **The export half answers first**, which is Ruby rather than a preference:
    /// `helper_method` defines its proxy *on* `_helpers` and `helper` includes a module *into*
    /// it, so where both hold a name the proxy is what runs.
    #[must_use]
    pub fn member(&self, graph: &Graph, member: &str) -> Option<Found> {
        let name = member.strip_suffix("()").unwrap_or(member);
        let id = StringId::from(member);
        if let Some(renderer) = &self.renderer
            && renderer.exported.contains(name)
            && let Ok(found) =
                query::find_member_in_ancestors(graph, renderer.declaration, id, false)
        {
            return Some(Found {
                declaration: found,
                how: InView::Exported(renderer.name.clone()),
            });
        }
        self.helpers
            .iter()
            .find_map(|module| query::find_member_in_ancestors(graph, *module, id, false).ok())
            .map(|found| Found {
                declaration: found,
                how: InView::Helper,
            })
    }

    /// Every member a bare word in this template could complete to, nearest first.
    ///
    /// The collecting half of the same walk, and it must collect rather than stop at the first
    /// answer: resolution takes one answer and completion takes all of them, so the two share
    /// the gate and not the loop.
    ///
    /// Deduplicated by name the way rubydex's own walk deduplicates: the nearest declaration of
    /// a name is the one that answers, so a second helper module spelling a name the first
    /// already spelled is not a second row.
    #[must_use]
    pub fn members(&self, graph: &Graph) -> Vec<Reached> {
        let mut seen: BTreeSet<StringId> = BTreeSet::new();
        let mut found: Vec<Reached> = Vec::new();
        if let Some(renderer) = &self.renderer {
            for name in &renderer.exported {
                let id = StringId::from(format!("{name}()").as_str());
                // Recorded whether or not it resolves, and never tested here: the export list
                // is a set, so it cannot repeat itself, and what this is for is the helpers
                // half below — 51 of the six corpora's 149 export sites name a method that is
                // also an `app/helpers` `def`, and one name is one row.
                seen.insert(id);
                if let Ok(member) =
                    query::find_member_in_ancestors(graph, renderer.declaration, id, false)
                {
                    found.push(Reached {
                        declaration: member,
                        step: EXPORTED,
                    });
                }
            }
        }
        for module in &self.helpers {
            for (_, namespace) in ancestors_of(graph, *module) {
                for (name, member) in namespace.members() {
                    // `include` installs methods and nothing else: a constant nested in a
                    // helper module is not reachable from a template through the view context.
                    if !matches!(
                        graph.declarations().get(member),
                        Some(Declaration::Method(_))
                    ) {
                        continue;
                    }
                    if seen.insert(*name) {
                        found.push(Reached {
                            declaration: *member,
                            step: HELPER,
                        });
                    }
                }
            }
        }
        found
    }
}

/// The same modules in the same order, each of them once.
///
/// A module can arrive twice — `helper ApplicationHelper` written in a controller, which the
/// `app/helpers` glob already found — and a module offered twice is a completion row offered
/// twice. `retain` over a `Vec` rather than a set, because the order is the answer's order and
/// the list is a few dozen long.
fn deduplicated(mut modules: Vec<DeclarationId>) -> Vec<DeclarationId> {
    let mut seen: BTreeSet<DeclarationId> = BTreeSet::new();
    modules.retain(|id| seen.insert(*id));
    modules
}

/// Where each half sits on the chain Rails builds `_helpers` from. See [`Reached::step`].
const EXPORTED: u16 = 0;
const HELPER: u16 = 1;

/// A namespace's linearized ancestors, itself first: the name each is filed under and the
/// members it holds.
///
/// Both callers want both halves — the export table is keyed by name and the completion list is
/// built out of members — so one walk answers them rather than two that would one day disagree
/// about which ancestors count. Nothing for a declaration that is not a namespace, which is a
/// refusal this module can only reach by being handed an id it did not put in its own table.
fn ancestors_of(graph: &Graph, declaration: DeclarationId) -> Vec<(&str, &Namespace)> {
    let Some(namespace) = graph
        .declarations()
        .get(&declaration)
        .and_then(Declaration::as_namespace)
    else {
        return Vec::new();
    };
    namespace
        .ancestors()
        .iter()
        .filter_map(|ancestor| match ancestor {
            // A rung rubydex could not linearize names no module, so there is nothing on it to
            // include and nothing on it to have exported.
            Ancestor::Complete(id) => Some(id),
            _ => None,
        })
        .filter_map(|id| {
            let found = graph.declarations().get(id)?;
            Some((found.name(), found.as_namespace()?))
        })
        .collect()
}
