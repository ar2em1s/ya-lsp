//! What a template can call, and where a bare word in one comes from.
//!
//! Every other answer in this crate starts from something the file under the cursor writes: a
//! receiver, a constant, a `def`. A template writes none of them. `<%= time_ago(story) %>` is a
//! call on an **implicit receiver** whose class Rails builds at render time from three things no
//! file names: every module under `app/helpers`, the `helper_method` proxies of the controller the
//! template's *path* implies, and ActionView's own helper modules. So rubydex correctly resolves it
//! to nothing.
//!
//! **A generated `module` holding the view context cannot work**, which is why this is a rung, not
//! a declaration. RBS cannot say *self in this file is X*, so the module would exist and nothing
//! would reach it. Wrapping the template's Ruby in a `class … end` is ruled out by construction,
//! because [`erb::ruby_view`] replaces markup with one space per byte so every offset rubydex
//! records is the template's own. And declaring the helpers half in RBS would give every helper
//! method in the project a second *place*.
//!
//! So it is one table, consulted by [`locator::resolve_typed`](super::locator::resolve_typed) where
//! every other rung is, and by `completion` at the same cursor. Both read one walk, as the concern
//! edge reads `locator::extended_modules`: resolution takes the first answer, completion collects
//! all, and the gate deciding what the view context *is* is written once, here.
//!
//! # The three halves
//!
//! - **`app/helpers`** needs no macro read. Rails globs `**/*_helper.rb` under each `app/helpers`
//!   directory and includes every module it finds, so the `def`s are already in the graph and only
//!   the edge is missing: [`rails::is_helper`] and a list of names. Of the two halves an
//!   application writes, this is by far the larger.
//! - **`helper_method`** is the machinery. `AbstractController::Helpers` writes `def current_user`
//!   per call onto the controller's `_helpers` module, so the macro hands over a *permission*, not
//!   a member: the `def` it names is already on the controller.
//! - **ActionView's own**, [`rails::VIEW_CONTEXT`], is the half no application writes, and the
//!   largest by call sites. `ActionView::Base` is `include Helpers, ::ERB::Util, Context`, and
//!   `ActionView::Helpers` `include`s its helper modules at module-body level, so **one name
//!   reaches all of them**, and rubydex's linearization (actionview is in the bundle, so already in
//!   the graph) does the walk. Nothing is generated or declared: this half is a name to start an
//!   ancestor walk from, and a project whose bundle lacks actionview has no such declaration and
//!   answers nothing, which is the whole of its gate.
//!
//! They are read in reverse order (export, then `app/helpers`, then ActionView), and that order is
//! Ruby's own: the proxy sits **on** `_helpers`, an application's module is `include`d into it, and
//! ActionView's were included into the view class before either, so `def tag` in
//! `ApplicationHelper` really does shadow `ActionView::Helpers::TagHelper#tag` in every template of
//! the project.
//!
//! # What the table refuses
//!
//! - **A name in none of the three.** `can?`, `policy`, a decorator's method: the rung is additive,
//!   so those answer on the name rung as before. They are a small residue of bare-word calls.
//! - **A controller method nobody exported.** `helper_method` is the whole gate on that half, per
//!   name, never per class: a template may call `current_user` because the class said so, and may
//!   not call `set_story` because it did not.
//! - **A mailer's views do not get the `app/helpers` half.** `include_all_helpers` is
//!   `ActionController::Base`'s default and `ActionMailer::Base` has none, so a mailer reaches an
//!   application helper only by writing `helper` itself. A mailer template gets its own exports,
//!   what it named, and ActionView's half, which every view context has.
//! - **A helper file does not get the export half.** A module under `app/helpers` is *in* the view
//!   context, not just read by it, so [`Views::reachable`] answers for one. But `helper_method` is
//!   a permission one controller grants, and a helper module is included into every controller's
//!   context, so there is no class to name and none is picked.
//!
//! Three bounds, stated up front:
//!
//! 1. **An engine's own files.** [`rails::is_helper`] and [`erb::is_template`] are asked of the
//!    path under the cursor, so a helper or template inside an indexed engine gets the
//!    *application's* helper modules in scope. Very few files hit this, so it stays a note here
//!    instead of this module reading `environment`.
//! 2. **Partials.** `rails::controller_of` reads the directory, not the file name, so
//!    `shared/_header.html.erb` names a `SharedController` nothing defines, and a partial under
//!    such a directory gets only the other two halves.
//! 3. **`include_all_helpers = false`.** An application setting it gets only its matching helper,
//!    and that config is out of reach for the same reason `database.yml` is.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

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
/// Both are conventions, not facts a file states, which puts this answer in the *derived* tier
/// beside the view↔renderer rung next to it: nothing in the template says which class renders it,
/// or that `app/helpers` is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InView {
    /// A `def` in a module under `app/helpers`, which Rails includes in every view context.
    Helper,
    /// A `helper_method` written by the class the template's path names, or by one of its
    /// ancestors. The name is the class Rails renders the template from.
    Exported(String),
    /// A `def` in one of the modules ActionView puts in every view context
    /// ([`rails::VIEW_CONTEXT`]). The outermost rung, so an application's own helper shadows it.
    Framework,
}

/// One member a bare word in a template reaches.
pub struct Reached {
    pub declaration: DeclarationId,
    /// How far out the installing module sits, on the chain Rails builds `_helpers` from.
    ///
    /// A `helper_method` proxy is written **on** `_helpers`, an application's helper module is
    /// `include`d into it, and ActionView's own were included into the view class before either. So
    /// each half is one step nearer than the next, which is also the order they answer in when
    /// several hold a name: Ruby's own answer for a module defining a method its includer also
    /// defines.
    pub step: u16,
}

/// What a bare name is reached through, and the declaration it names.
pub struct Found {
    pub declaration: DeclarationId,
    pub how: InView,
}

/// Which modules and exports a template can see, rebuilt on every settle.
///
/// Held beside the graph for [`super::synthesized::Synthesized`]'s reason and filled by the same
/// pass: both halves are projections of what the generators already walked, and neither is a
/// declaration. `Default` is a workspace without Rails, which answers nothing and costs a path test
/// per hover to say so.
#[derive(Debug, Default)]
pub struct Views {
    /// Every `app/helpers/**/*_helper.rb` module the user's own code defines, fully spelled and
    /// sorted, so two helper modules spelling one name answer the same way every run.
    helpers: Vec<String>,
    /// A body that wrote a `helper_method`, and the names it handed over.
    ///
    /// Keyed by the class or module the macro is written in, not the controller it reaches, because
    /// a concern does not know its includers and need not: the ancestor walk below crosses the
    /// `include` the controller already wrote.
    exports: BTreeMap<String, BTreeSet<String>>,
    /// A body that wrote a `helper`, and the modules it put into its own view context.
    ///
    /// The other half of the mailer story, and why the mailer gate below is a bound, not a wall.
    /// `ActionMailer::Base` has no `include_all_helpers`, so a mailer reaches an application helper
    /// only by naming it, and most `helper` calls are in mailers. In a controller one usually adds
    /// nothing new, except when it names a module a gem ships, which the `app/helpers` glob does
    /// not reach either.
    included: BTreeMap<String, BTreeSet<String>>,
    /// The application classes a **mailer's** view directory may name.
    ///
    /// A second gate, not a widening of the first: `rails::controller_of` produces a name only a
    /// controller has, while `rails::mailer_of` produces whatever the directory spells
    /// (`app/views/shared/` spells `Shared`), so the second is only ever checked against this list.
    mailers: BTreeSet<String>,
    /// Whether this project wants a view context at all: `[rails] views`.
    ///
    /// A flag, not an empty map, because the module's two halves fail differently when empty:
    /// `named_by` answers nothing, which is harmless, but the **renderer** half asks
    /// `rails::controller_of` of a path and would keep citing a controller that does not exist. A
    /// Sinatra or Hanami application with an `app/views/` is exactly the project that must be able
    /// to say no, and an empty map cannot say it.
    ///
    /// So `Views::default()` is **off**, which is also what the pass leaves when the switch says
    /// so.
    enabled: bool,
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
            enabled: true,
        }
    }

    /// What the document filed under `uri_id` can call, or nothing.
    ///
    /// `None` for any document that is neither a template nor a helper: the first test, and the
    /// cheap one. This is asked of every call rubydex could not resolve, so an ordinary `.rb` file
    /// must pay a path test and no more. Also `None` where all three halves are empty, so a Rails
    /// application with no helpers, no exports and no actionview in its bundle pays nothing
    /// further.
    ///
    /// **A module under `app/helpers` is *in* the view context, not just read by it.** Rails
    /// includes all of them into the same `_helpers`, so a bare call in one reaches the other
    /// helper modules and ActionView's exactly as a template's does, and a helper file is the only
    /// other place in an application where that holds. It does **not** get the export half:
    /// `helper_method` is a permission one controller grants, and a helper module is included into
    /// every controller's view context, so there is no class for [`rails::controller_of`] to name
    /// and no honest way to pick one. That is why the renderer lookup below sits inside the
    /// template arm.
    #[must_use]
    pub fn reachable(&self, graph: &Graph, uri_id: UriId) -> Option<Reachable> {
        if !self.enabled {
            return None;
        }
        let path = DocUri::from_graph_uri(graph.documents().get(&uri_id)?.uri())?.to_file_path()?;
        let template = erb::is_template(&path);
        if !template && !rails::is_helper(&path) {
            return None;
        }

        let renderer = self.rendered_by(graph, &path);
        let named = renderer
            .as_ref()
            .map(|rendered| self.named_by(graph, rendered.declaration))
            .unwrap_or_default();
        let renderer = renderer.map(|rendered| Renderer {
            exported: self.exported_by(graph, rendered.declaration),
            name: rendered.name,
            declaration: rendered.declaration,
            controller: rendered.controller,
        });
        // A mailer's own views are the one place the glob does not apply (see the module docs),
        // while a template whose class does not exist still gets it, which is what a partial under
        // `shared/` relies on. A mailer gets instead what it asked for by name, which is `helper`'s
        // whole purpose.
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

        // The framework's half, which every view context gets and no file asks for: a mailer's
        // template, a partial under `shared/` with no controller, and a helper module all render
        // through an `ActionView::Base`. Resolved, not declared: actionview is in the bundle and so
        // already in the graph, and an application without it answers nothing here, with no switch
        // needed.
        let framework = rails::VIEW_CONTEXT
            .iter()
            .filter_map(|name| declared(graph, name))
            .collect();

        let reachable = Reachable {
            renderer,
            helpers,
            framework,
        };
        (!reachable.is_empty()).then_some(reachable)
    }

    /// Which class Rails renders `path` from: the controller, or the mailer where there is no
    /// controller.
    ///
    /// **The order is Rails' own.** `app/views/user_mailer/` names a `UserMailerController` that
    /// does not exist, so the mailer is reached exactly where the controller is not, and an
    /// application that *does* define a `UserMailerController` has said something this rule must
    /// not override.
    ///
    /// **The second half is gated; the first need not be.** [`rails::controller_of`] produces a
    /// name only a controller has; [`rails::mailer_of`] produces whatever the directory spells
    /// (`app/views/shared/` spells `Shared`), so it answers only for an application class
    /// [`rails::is_mailer`] recognises. That gate is [`Views::mailers`](Views), filled by the pass
    /// from the superclasses it already holds.
    ///
    /// Public because this is the one convention two modules read, and they must agree: here it
    /// decides what a template may **call**, and in [`types`](super::types) where a template's
    /// `@ivar` was **written** and which class a card names. A view context built from a mailer
    /// beside a card citing a controller would be two answers about one path.
    ///
    /// `None` for any path that is not a template, so a caller holding a path need not check, and
    /// `None` for everything when `[rails] views` is off.
    #[must_use]
    pub fn rendered_by(&self, graph: &Graph, path: &Path) -> Option<RenderedBy> {
        if !self.enabled || !erb::is_template(path) {
            return None;
        }
        rails::controller_of(path)
            .and_then(|name| {
                Some(RenderedBy {
                    declaration: declared(graph, &name)?,
                    name,
                    controller: true,
                })
            })
            .or_else(|| {
                let name = rails::mailer_of(path)?;
                if !self.mailers.contains(&name) {
                    return None;
                }
                Some(RenderedBy {
                    declaration: declared(graph, &name)?,
                    name,
                    controller: false,
                })
            })
    }

    /// Every module `declaration` or one of its ancestors named with `helper`.
    ///
    /// The ancestor walk is [`Views::exported_by`]'s and matters the same way:
    /// `helper :application` written once, in `ApplicationMailer`, is meant for every mailer under
    /// it.
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
    /// The ancestor walk makes three of the macro's hosts work without special cases: a
    /// `helper_method` in a concern, in an `app/helpers` module the controller `include`s, and in
    /// `ApplicationController` are all one question (is the class this template's path names below
    /// the body that wrote the macro?), which rubydex answered at index time.
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

/// The class a template's path names, resolved against the graph.
///
/// Public because two modules ask the same question of one path; see [`Views::rendered_by`]. Each
/// uses the answer its own way: here the declaration is an ancestry to walk for what the template
/// may call, and in [`types`](super::types) it is the documents that class is written in, with the
/// name as the `self` an `@story` must be written under to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBy {
    /// The class, fully qualified, as the path spells it.
    pub name: String,
    pub declaration: DeclarationId,
    /// Whether a controller answered rather than a mailer.
    ///
    /// The one thing the two spellings decide differently, and both readers need it: a view context
    /// gets the `app/helpers` glob only from a controller, and a card must say which convention it
    /// followed. "The mailer Rails renders this template from" is a different sentence, and calling
    /// a mailer a controller would make the card wrong about the one fact it cites.
    pub controller: bool,
}

/// The class a template's path names, and what it lets a template call.
struct Renderer {
    name: String,
    declaration: DeclarationId,
    /// Whether it is a controller rather than a mailer ([`RenderedBy::controller`]); here it
    /// switches the `app/helpers` glob.
    controller: bool,
    exported: BTreeSet<String>,
}

/// One template's view context, resolved against the graph.
pub struct Reachable {
    renderer: Option<Renderer>,
    helpers: Vec<DeclarationId>,
    /// The framework's own half: whichever of [`rails::VIEW_CONTEXT`] the graph holds.
    ///
    /// Empty for a project whose bundle lacks actionview, which is this half's whole gate: no
    /// switch and no path test, because a module missing from the graph cannot be walked, and one
    /// present could only have come from the bundle.
    framework: Vec<DeclarationId>,
}

impl Reachable {
    fn is_empty(&self) -> bool {
        self.helpers.is_empty()
            && self.framework.is_empty()
            && self
                .renderer
                .as_ref()
                .is_none_or(|renderer| renderer.exported.is_empty())
    }

    /// The declaration a bare `member` written in this template names.
    ///
    /// `member` is rubydex's parenthesised spelling, because that is what the caller has and what
    /// the lookup needs; the export list holds the symbols the macro was written with, so the
    /// parentheses come off for that test only.
    ///
    /// **The export half answers first**, which is Ruby, not preference: `helper_method` defines
    /// its proxy *on* `_helpers` and `helper` includes a module *into* it, so where both hold a
    /// name, the proxy is what runs.
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
        if let Some(found) = self
            .helpers
            .iter()
            .find_map(|module| query::find_member_in_ancestors(graph, *module, id, false).ok())
        {
            return Some(Found {
                declaration: found,
                how: InView::Helper,
            });
        }
        // Last, which is what keeps this half additive: an application writing `def tag` in
        // `ApplicationHelper` has shadowed `ActionView::Helpers::TagHelper#tag` for all its
        // templates, and the two lookups above have already answered by the time this is asked.
        self.framework
            .iter()
            .find_map(|module| query::find_member_in_ancestors(graph, *module, id, false).ok())
            .map(|found| Found {
                declaration: found,
                how: InView::Framework,
            })
    }

    /// Every member a bare word in this template could complete to, nearest first.
    ///
    /// The collecting half of the same walk, and it must collect instead of stopping at the first
    /// answer: resolution takes one answer and completion all of them, so they share the gate, not
    /// the loop.
    ///
    /// Deduplicated by name as rubydex's own walk does: the nearest declaration of a name is the
    /// one that answers, so a second helper module spelling a name the first already spelled is not
    /// a second row.
    #[must_use]
    pub fn members(&self, graph: &Graph) -> Vec<Reached> {
        let mut seen: BTreeSet<StringId> = BTreeSet::new();
        let mut found: Vec<Reached> = Vec::new();
        if let Some(renderer) = &self.renderer {
            for name in &renderer.exported {
                let id = StringId::from(format!("{name}()").as_str());
                // Recorded whether or not it resolves, and never tested here: the export list is a
                // set, so it cannot repeat itself. What this is for is the helpers half below:
                // exports often name a method that is also an `app/helpers` `def`, and one name is
                // one row.
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
        for (modules, step) in [(&self.helpers, HELPER), (&self.framework, FRAMEWORK)] {
            for module in modules {
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
                        // **Visibility is deliberately not read here.** A good share of
                        // ActionView's helper `def`s are private, so filtering would drop many of
                        // the rows this half adds. It is `completion`'s own rule: a cursor with no
                        // written receiver sets `private_ok`, because Ruby really lets an implicit
                        // receiver call a private method. Filtering also lost more rows than it
                        // removed in real template lists, because applications' own helper modules
                        // mark many methods private and every one was already offered.
                        if seen.insert(*name) {
                            found.push(Reached {
                                declaration: *member,
                                step,
                            });
                        }
                    }
                }
            }
        }
        found
    }
}

/// The same modules in the same order, each once.
///
/// A module can arrive twice (`helper ApplicationHelper` in a controller, already found by the
/// `app/helpers` glob), and a module offered twice is a completion row offered twice. `retain` over
/// a `Vec`, not a set, because the order is the answer's order and the list is a few dozen long.
fn deduplicated(mut modules: Vec<DeclarationId>) -> Vec<DeclarationId> {
    let mut seen: BTreeSet<DeclarationId> = BTreeSet::new();
    modules.retain(|id| seen.insert(*id));
    modules
}

/// Where each half sits on the chain Rails builds `_helpers` from. See [`Reached::step`].
///
/// The order is Ruby's own: the proxy `helper_method` writes sits **on** `_helpers`, an
/// application's helper module is `include`d into it, and ActionView's modules were included into
/// the view class before either, so the framework half is furthest away and the one an application
/// shadows by writing its own `def` of the same name.
const EXPORTED: u16 = 0;
const HELPER: u16 = 1;
const FRAMEWORK: u16 = 2;

/// A namespace's linearized ancestors, itself first: the name each is filed under and the members
/// it holds.
///
/// Both callers want both halves (the export table is keyed by name; the completion list is built
/// from members), so one walk answers both instead of two that would one day disagree about which
/// ancestors count. Nothing for a non-namespace declaration, a refusal this module reaches only if
/// handed an id it did not put in its own table.
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
            // include or to have exported.
            Ancestor::Complete(id) => Some(id),
            _ => None,
        })
        .filter_map(|id| {
            let found = graph.declarations().get(id)?;
            Some((found.name(), found.as_namespace()?))
        })
        .collect()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use rubydex::model::declaration::MethodDeclaration;

    use super::*;
    use crate::analysis::testing::*;

    /// A controller, a helper and a template, in the layout the view context reads.
    ///
    /// The controller exports one of its two methods, on purpose: `helper_method` is a permission
    /// and the whole gate on that half, so a fixture with one method could not tell "this template
    /// may call it" from "this project defines it once".
    fn view_context_app(harness: &Harness) -> DocUri {
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  helper_method :current_user\n\n  def current_user\n  end\n\n  def set_story\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write("app/views/stories/show.html.erb", "<%= current_user %>\n")
    }

    /// The shape actionview ships, small enough to read and exact where it matters.
    ///
    /// Written as workspace Ruby, not pulled from a bundle, because this half needs a *graph* from
    /// the gem, not a file: `ActionView::Helpers` `include`s its helper modules at module-body
    /// level, so rubydex linearizes them and one name reaches all. The three `def`s are the three
    /// the tests ask for, and `t` is an alias of `translate` because that is how the real
    /// `TranslationHelper` writes the commonest call in any Rails application.
    fn action_view(harness: &Harness) {
        harness.write(
            "lib/action_view/helpers.rb",
            "module ActionView\n  module Helpers\n    module TagHelper\n      \
             def tag(name)\n      end\n    end\n\n    module UrlHelper\n      \
             def link_to(text, url)\n      end\n    end\n\n    module TranslationHelper\n      \
             def translate(key)\n      end\n      alias t translate\n    end\n\n    \
             include TagHelper\n    include UrlHelper\n    include TranslationHelper\n  \
             end\nend\n",
        );
        harness.write(
            "lib/erb/util.rb",
            "module ERB\n  module Util\n    def h(text)\n    end\n  end\nend\n",
        );
    }

    #[test]
    fn a_declaration_that_is_not_a_namespace_has_no_ancestors() {
        // Neither id below can reach `ancestors_of` from a request: both callers pass a key from a
        // table this module filled with namespaces itself. The refusal keeps a wrong id from
        // reading as a namespace with no members, which would answer an *empty view context* (a
        // template offered nothing) instead of a declined answer. A fabricated graph is the only
        // way to pin it, for `indexed`'s reason: no real project has the shape.
        let mut graph = Graph::default();
        let object = DeclarationId::from("Object");
        graph.declarations_mut().insert(
            DeclarationId::from("ApplicationHelper#time_ago()"),
            Declaration::Method(Box::new(MethodDeclaration::new(
                "ApplicationHelper#time_ago()".to_string(),
                object,
            ))),
        );
        assert!(
            ancestors_of(&graph, DeclarationId::from("ApplicationHelper#time_ago()")).is_empty(),
            "a method is not a namespace"
        );
        assert!(
            ancestors_of(&graph, DeclarationId::from("ApplicationHelper")).is_empty(),
            "nor is a name the graph does not hold"
        );
    }

    #[test]
    fn a_project_that_is_not_rails_can_say_so_and_the_view_context_goes_quiet() {
        // **The case `[rails] views` exists for**, and it is about a wrong answer, not a slow one:
        // `rails::controller_of` reads a *path*, so any project with an `app/views/` gets Rails'
        // convention applied, including a Sinatra or Hanami application, where the controller the
        // card cites does not exist.
        //
        // Off, the same cursor falls to the rung below: the answer the workspace would give if
        // nobody had written a controller.
        let mut harness = Harness::configured("[rails]\nviews = false\n");
        let view = view_context_app(&harness);
        harness.index();

        let source = "<%= current_user %>\n";
        let fallen = card(&mut harness, &view, source, "current_user");
        assert!(
            fallen.contains("method name alone"),
            "the view context is still answering: {fallen}"
        );

        // **What the fixture cannot show and the tier can.** There is one `current_user` in the
        // project, so the name rung finds the same method either way, which is the point: the
        // difference a user sees is the *tier*. With the view context off, this is a guess instead
        // of a card reached through the class the path names.
        let mut harness = Harness::new();
        let view = view_context_app(&harness);
        harness.index();
        let cited = card(&mut harness, &view, source, "current_user");
        assert!(cited.contains("StoriesController#current_user"), "{cited}");
        assert!(!cited.contains("method name alone"), "{cited}");
    }

    #[test]
    fn a_helper_method_export_is_what_a_template_may_call() {
        // The view context, first clause. `current_user` in a template jumps anyway (there is one
        // method of that name in the project) on the *name* rung, the same answer as if the
        // controller had never exported it. What changes is the tier and the gate: the answer is
        // reached through the class the path names, and a method the class did **not** export is
        // not reachable at all.
        let mut harness = Harness::new();
        let view = view_context_app(&harness);
        harness.index();

        let source = "<%= current_user %>\n";
        let exported = card(&mut harness, &view, source, "current_user");
        assert!(
            exported.contains("StoriesController#current_user"),
            "{exported}"
        );
        assert!(
            exported.contains(
                "Reached through `helper_method` in `StoriesController` — the class Rails \
                 renders this template from."
            ),
            "{exported}"
        );
        assert!(
            !exported.contains("Matched on the method name alone"),
            "a convention that names a class is not a name match: {exported}"
        );

        let jump = harness.definition_at(&view, source, "current_user");
        assert!(
            jump[0]["targetUri"]
                .as_str()
                .unwrap_or_default()
                .ends_with("stories_controller.rb"),
            "{jump}"
        );

        // The obvious half is what a two-example probe can see; this is the other one, and it is
        // much larger in real projects.
        let offered = harness.declarations_at(&view, "<%= curr~ %>\n");
        assert!(offered.contains(&"current_user".to_owned()), "{offered:?}");

        // And the method nobody exported is not in the view context, in either request. It is still
        // *findable* (one `set_story` in the project, so the name rung answers), and the card says
        // so: the difference this gate exists to keep.
        let unexported = "<%= set_story %>\n";
        let other = harness.write("app/views/stories/edit.html.erb", unexported);
        harness.watch(&[&other]);
        let private = card(&mut harness, &other, unexported, "set_story");
        assert!(
            private.contains("Matched on the method name alone"),
            "`helper_method` is the gate, per name: {private}"
        );
        let offered = harness.declarations_at(&other, "<%= set_s~ %>\n");
        assert!(!offered.contains(&"set_story".to_owned()), "{offered:?}");
    }

    #[test]
    fn an_export_this_cannot_read_a_name_out_of_hands_over_nothing() {
        // The direction every reader in `workspace/rails` errs in, asked of the one macro whose
        // answer is a permission, not a member. A splat and an interpolated symbol are Ruby that
        // only runs, and a bare `helper_method` is a no-op Rails accepts, so all three export
        // nothing, and `render_story` keeps its previous answer instead of becoming callable
        // because a call of the right name was written.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  EXPORTS = [:render_story]\n  helper_method\n  \
             helper_method(*EXPORTS)\n  helper_method :\"#{prefix}_story\"\n\n  \
             def render_story\n  end\nend\n",
        );
        let source = "<%= render_story %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let card = card(&mut harness, &view, source, "render_story");
        assert!(
            card.contains("Matched on the method name alone"),
            "a name no literal spelled is a name nobody exported: {card}"
        );
    }

    #[test]
    fn every_app_helpers_module_is_in_every_template() {
        // The other half, which needs no macro: Rails' `all_helpers_from_path` globs
        // `app/helpers/**/*_helper.rb` and includes every module it finds in every view context. So
        // this template's controller does not exist (`app/views/comments/` names a
        // `CommentsController` this application never wrote), and the helpers answer anyway, which
        // is what most partials rely on.
        let mut harness = Harness::new();
        view_context_app(&harness);
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def byline\n  end\nend\n",
        );
        // Under `app/helpers` but not named as Rails' glob names them: solidus'
        // `controller_helpers/auth.rb` shape, reached by an `include` a controller writes, and in
        // no view context by default.
        harness.write(
            "app/helpers/legacy/auth.rb",
            "module Legacy\n  module Auth\n    def sign_out\n    end\n  end\nend\n",
        );
        let source = "<%= time_ago(1) %> <%= sign_out %>\n";
        let view = harness.write("app/views/comments/index.html.erb", source);
        harness.index();

        let globbed = card(&mut harness, &view, source, "time_ago");
        assert!(globbed.contains("ApplicationHelper#time_ago"), "{globbed}");
        assert!(
            globbed.contains(
                "Reached through the view context — Rails includes every `app/helpers` module \
                 in it."
            ),
            "{globbed}"
        );

        let unglobbed = card(&mut harness, &view, source, "sign_out");
        assert!(
            unglobbed.contains("Matched on the method name alone"),
            "a file Rails' own glob does not name is in no view context: {unglobbed}"
        );

        // Every module, not just the one matching the directory: `include_all_helpers` is the Rails
        // default, and an application turning it off is a bound this states, not reads.
        let offered = harness.declarations_at(&view, "<%= ~ %>\n");
        for name in ["time_ago", "byline"] {
            assert!(offered.contains(&name.to_owned()), "{name}: {offered:?}");
        }
        assert!(!offered.contains(&"sign_out".to_owned()), "{offered:?}");
    }

    #[test]
    fn where_the_two_halves_meet_the_export_wins_and_the_row_is_not_offered_twice() {
        // Three refusals and one precedence, in one fixture, because they are one question.
        //
        // **The export wins.** `helper_method` writes its proxy *on* `_helpers` and `helper`
        // includes a module *into* it, so where both hold a name, the proxy runs. Exports often
        // name a method that is also an `app/helpers` `def`, so this is the commonest shape the two
        // halves share, not an edge case.
        //
        // **One name is one row.** The same pair in a completion list would be one word twice, one
        // of which jumps somewhere the call would not go.
        //
        // **An export naming nothing declares nothing.** `helper_method :missing` permits a method
        // that does not exist, so the half below answers instead.
        //
        // **A constant in a helper module is not in the view context.** `include` installs methods;
        // `ApplicationHelper::MAX` is reached by writing it out.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  helper_method :current_user, :missing\n\n  \
             def current_user\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  MAX = 5\n\n  def current_user\n  end\n\n  \
             def time_ago(at)\n  end\nend\n",
        );
        // Somewhere the view context cannot reach, so the export declining shows up as the rung
        // below answering, not as an empty hover.
        harness.write("lib/tools.rb", "module Tools\n  def missing\n  end\nend\n");
        let source = "<%= current_user %> <%= missing %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let shared = card(&mut harness, &view, source, "current_user");
        assert!(
            shared.contains("StoriesController#current_user"),
            "the proxy is written on `_helpers` and the module is included into it: {shared}"
        );
        assert!(
            shared.contains("`helper_method` in `StoriesController`"),
            "{shared}"
        );

        let unwritten = card(&mut harness, &view, source, "missing");
        assert!(
            unwritten.contains("Matched on the method name alone"),
            "a permission for a method nobody wrote is not an answer: {unwritten}"
        );

        let offered = harness.declarations_at(&view, "<%= ~ %>\n");
        assert_eq!(
            offered
                .iter()
                .filter(|label| *label == "current_user")
                .count(),
            1,
            "one name is one row: {offered:?}"
        );
        assert!(offered.contains(&"time_ago".to_owned()), "{offered:?}");
        assert!(
            !offered.contains(&"MAX".to_owned()),
            "`include` installs methods and nothing else: {offered:?}"
        );
    }

    #[test]
    fn an_export_written_in_a_concern_reaches_the_controllers_that_include_it() {
        // Exports are often written in a concern, or in an `app/helpers` module a controller
        // `include`s, so a reader that walked `app/controllers` and keyed by class would miss most
        // of them. Nothing here knows what a concern is: the export list is keyed by the body that
        // wrote the macro, and the walk from the template's class up its ancestors crosses the
        // `include` the controller already wrote.
        let mut harness = Harness::new();
        harness.write(
            "app/controllers/concerns/authentication.rb",
            "module Authentication\n  extend ActiveSupport::Concern\n\n  included do\n    \
             helper_method :current_user\n  end\n\n  def current_user\n  end\nend\n",
        );
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  include Authentication\nend\n",
        );
        let source = "<%= current_user %>\n";
        let view = harness.write("app/views/stories/show.html.erb", source);
        harness.index();

        let through = card(&mut harness, &view, source, "current_user");
        assert!(through.contains("Authentication#current_user"), "{through}");
        // The class the *template* names, not the module the macro is in: what a reader must be
        // able to check is that Rails renders this template from there.
        assert!(
            through.contains("`helper_method` in `StoriesController`"),
            "{through}"
        );
    }

    #[test]
    fn a_mailer_gets_its_own_exports_and_not_the_applications_helpers() {
        // `AbstractController::Helpers` is in `ActionMailer::Base` too, so `helper_method` in a
        // mailer is real, and the view path a mailer renders from is its name with no `Controller`
        // suffix. `include_all_helpers` is **not** there: it is `ActionController::Base`'s default,
        // and a mailer reaches an application helper only by writing `helper` itself, which is a
        // separate call. So the two halves separate here and nowhere else.
        let mut harness = Harness::new();
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  helper_method :sender_name\n\n  \
             def sender_name\n  end\nend\n",
        );
        let source = "<%= sender_name %> <%= time_ago(1) %>\n";
        let view = harness.write("app/views/user_mailer/welcome.html.erb", source);
        harness.index();

        let exported = card(&mut harness, &view, source, "sender_name");
        assert!(exported.contains("UserMailer#sender_name"), "{exported}");
        assert!(
            exported.contains("`helper_method` in `UserMailer`"),
            "{exported}"
        );

        let helper = card(&mut harness, &view, source, "time_ago");
        assert!(
            helper.contains("Matched on the method name alone"),
            "a mailer's views are not a controller's: {helper}"
        );

        // The way back in is the one Rails provides: `helper :application` names the module, and a
        // mailer naming it has said the only thing that puts an application helper in front of a
        // mailer template. Most real `helper` calls are in mailers, and they are the whole of the
        // gap.
        let mailer = harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ApplicationMailer\n  helper :application\n  \
             helper_method :sender_name\n\n  def sender_name\n  end\nend\n",
        );
        harness.watch(&[&mailer]);
        let named = card(&mut harness, &view, source, "time_ago");
        assert!(named.contains("ApplicationHelper#time_ago"), "{named}");
        assert!(
            named.contains("Reached through the view context"),
            "{named}"
        );

        // And the directory must name a **mailer**: `mailer_of` spells whatever the path spells, so
        // the gate is the application's own superclass table, not the path.
        let shared = "<%= sender_name %>\n";
        let other = harness.write("app/views/shared/_footer.html.erb", shared);
        harness.watch(&[&other]);
        let elsewhere = card(&mut harness, &other, shared, "sender_name");
        assert!(
            elsewhere.contains("Matched on the method name alone"),
            "`app/views/shared/` names `Shared`, which renders nothing: {elsewhere}"
        );
    }

    #[test]
    fn the_view_context_is_additive_and_reaches_no_further_than_a_template() {
        // Two refusals in one fixture, both about safety, not features.
        //
        // **Additive.** The view context is three halves ordered as Ruby orders them: an
        // application may write `def tag` in `ApplicationHelper` on purpose, to shadow
        // `ActionView::Helpers::TagHelper#tag`, so a template's `tag` must reach the application's,
        // while `link_to`, which the application does not redefine, reaches ActionView's. The
        // application module is `include`d into `_helpers` and ActionView's were included into the
        // view class before it, so "nearer wins" is the whole rule.
        //
        // **A template and nothing else.** The same bare call in a `.rb` file that is not a helper
        // is an ordinary receiverless call whose `self` is `main`, and Rails puts no helpers there.
        let mut harness = Harness::new();
        action_view(&harness);
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def tag(name)\n  end\nend\n",
        );
        let source = "<%= tag(:p) %> <%= link_to(\"x\", \"/\") %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        let script = "tag(:p)\n";
        let plain = harness.write("bin/report.rb", script);
        harness.index();

        let own = card(&mut harness, &view, source, "tag");
        assert!(own.contains("ApplicationHelper#tag"), "{own}");
        assert!(!own.contains("Matched on the method name alone"), "{own}");

        let shipped = card(&mut harness, &view, source, "link_to");
        assert!(
            shipped.contains("ActionView::Helpers::UrlHelper#link_to"),
            "{shipped}"
        );
        assert!(
            shipped.contains(
                "Reached through the view context — ActionView includes its own helper \
                 modules in it."
            ),
            "{shipped}"
        );

        let outside = card(&mut harness, &plain, script, "tag");
        assert!(
            outside.contains("Matched on the method name alone"),
            "nothing outside a template changes: {outside}"
        );
    }

    #[test]
    fn a_name_in_none_of_the_three_halves_still_falls_through() {
        // The rung is additive at its own edge too, as this asserts: `can?` is cancan's, `policy`
        // is pundit's, and neither is in any half. Every such call must keep the answer it had
        // before this half existed.
        let mut harness = Harness::new();
        action_view(&harness);
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write(
            "lib/cancan.rb",
            "module CanCan\n  def can?(action)\n  end\nend\n",
        );
        let source = "<%= can?(:read) %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        let fallen = card(&mut harness, &view, source, "can?");
        assert!(
            fallen.contains("Matched on the method name alone"),
            "a name the view context does not hold falls through, it is not swallowed: {fallen}"
        );
    }

    #[test]
    fn a_project_whose_bundle_ships_no_actionview_answers_nothing_from_that_half() {
        // The third half's whole gate, deliberately not a switch: the half is two *names*, and a
        // name the graph lacks cannot be walked. So a project with an `app/views/` and no Rails in
        // its bundle (the Sinatra application `[rails] views` exists for, or a Rails application
        // whose gems are not installed yet) gets the two halves it has evidence for, with the name
        // rung under them.
        let mut harness = Harness::new();
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        harness.write(
            "lib/markup.rb",
            "module Markup\n  def link_to(text, url)\n  end\nend\n",
        );
        let source = "<%= time_ago(1) %> <%= link_to(\"x\", \"/\") %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        let own = card(&mut harness, &view, source, "time_ago");
        assert!(own.contains("ApplicationHelper#time_ago"), "{own}");

        let missing = card(&mut harness, &view, source, "link_to");
        assert!(
            missing.contains("Matched on the method name alone"),
            "nothing is invented for a gem that is not there: {missing}"
        );
    }

    #[test]
    fn a_module_under_app_helpers_is_in_the_view_context_rather_than_read_by_it() {
        // Helper files need this as much as templates: Rails includes every helper module into the
        // same `_helpers`, so a bare call in one reaches the other helper modules and ActionView's
        // own exactly as a template's does.
        //
        // **It does not get the export half.** `helper_method` is a permission one controller
        // grants; a helper module is included into every controller's view context, so there is no
        // class for `rails::controller_of` to name and none is picked. That is also the one thing
        // that would be wrong if this lane simply reused the template's.
        //
        // **And Rails' own glob still decides.** `app/helpers/legacy/auth.rb` is not named
        // `*_helper.rb`, so it is in no view context and nothing changes for it.
        let mut harness = Harness::new();
        action_view(&harness);
        harness.write(
            "app/controllers/stories_controller.rb",
            "class StoriesController\n  helper_method :current_user\n\n  \
             def current_user\n  end\nend\n",
        );
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def byline\n  end\nend\n",
        );
        let source = "module ApplicationHelper\n  def summary\n    link_to(byline, \"/\")\n    \
                      current_user\n  end\nend\n";
        let helper = harness.write("app/helpers/application_helper.rb", source);
        let unglobbed_source = "module Legacy\n  module Auth\n    def out\n      \
                                link_to(\"x\", \"/\")\n    end\n  end\nend\n";
        let unglobbed = harness.write("app/helpers/legacy/auth.rb", unglobbed_source);
        harness.index();

        let shipped = card(&mut harness, &helper, source, "link_to");
        assert!(
            shipped.contains("ActionView::Helpers::UrlHelper#link_to"),
            "{shipped}"
        );

        let sibling = card(&mut harness, &helper, source, "byline");
        assert!(sibling.contains("StoriesHelper#byline"), "{sibling}");

        let exported = card(&mut harness, &helper, source, "current_user");
        assert!(
            exported.contains("Matched on the method name alone"),
            "a helper module belongs to no one controller, so it is granted no permission by \
             one: {exported}"
        );

        let outside = card(&mut harness, &unglobbed, unglobbed_source, "link_to");
        assert!(
            outside.contains("Matched on the method name alone"),
            "Rails' own glob decides which file is a helper, here as everywhere: {outside}"
        );
    }

    #[test]
    fn a_mailer_template_gets_the_framework_half_it_does_not_get_the_applications() {
        // The half every view context has, checked against the template with the least:
        // `include_all_helpers` is `ActionController::Base`'s and `ActionMailer::Base` has none, so
        // a mailer reaches an application helper only by naming it, but every renderer builds an
        // `ActionView::Base`, so `link_to` and `t` work in a mailer view. A partial under
        // `shared/`, with no controller at all, has the same shape, which is why this half reaches
        // the residue the other two leave.
        let mut harness = Harness::new();
        action_view(&harness);
        harness.write(
            "app/mailers/user_mailer.rb",
            "class UserMailer < ActionMailer::Base\n  def welcome\n  end\nend\n",
        );
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        let source = "<%= link_to(\"x\", \"/\") %> <%= time_ago(1) %>\n";
        let mailer = harness.write("app/views/user_mailer/welcome.html.erb", source);
        let orphan = harness.write("app/views/shared/_header.html.erb", source);
        harness.index();

        for view in [&mailer, &orphan] {
            let shipped = card(&mut harness, view, source, "link_to");
            assert!(
                shipped.contains("ActionView::Helpers::UrlHelper#link_to"),
                "{shipped}"
            );
        }

        let refused = card(&mut harness, &mailer, source, "time_ago");
        assert!(
            refused.contains("Matched on the method name alone"),
            "a mailer gets no application helper it did not name: {refused}"
        );
        let globbed = card(&mut harness, &orphan, source, "time_ago");
        assert!(globbed.contains("ApplicationHelper#time_ago"), "{globbed}");
    }

    #[test]
    fn erb_util_is_the_second_name_and_it_is_worth_its_row() {
        // `ActionView::Base` is `include Helpers, ::ERB::Util, Context`, and the middle one is by
        // far the smallest: in practice `h` and `json_escape`. It is here because `h` is a template
        // idiom and the row costs one name. `Context` is not, for the same reason read the other
        // way: `output_buffer` and `view_flow` are renderer plumbing no template calls.
        let mut harness = Harness::new();
        action_view(&harness);
        let source = "<%= h(story.title) %>\n";
        let view = harness.write("app/views/stories/index.html.erb", source);
        harness.index();

        let escaped = card(&mut harness, &view, source, "h");
        assert!(escaped.contains("ERB::Util#h"), "{escaped}");
    }

    #[test]
    fn the_framework_half_is_offered_in_completion_and_ranks_under_the_applications_own() {
        // The collecting half of the same walk. `Reached::step` carries the chain Rails builds
        // `_helpers` from (export 0, `app/helpers` 1, ActionView 2), so a template offers the word
        // the application wrote above the one the framework ships, the order in which the two would
        // run.
        let mut harness = Harness::new();
        action_view(&harness);
        harness.write(
            "app/helpers/application_helper.rb",
            "module ApplicationHelper\n  def time_ago(at)\n  end\nend\n",
        );
        let view = harness.write("app/views/stories/index.html.erb", "<%= time_ago(1) %>\n");
        harness.index();

        let offered = harness.declarations_at(&view, "<%= ~ %>\n");
        for name in ["time_ago", "link_to", "translate", "h"] {
            assert!(offered.contains(&name.to_owned()), "{name}: {offered:?}");
        }
        let own = offered.iter().position(|label| label == "time_ago");
        let shipped = offered.iter().position(|label| label == "link_to");
        assert!(own < shipped, "{own:?} then {shipped:?}: {offered:?}");
    }
}
