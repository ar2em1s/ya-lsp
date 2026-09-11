//! What a template can call, and where a bare word in one comes from.
//!
//! Every other answer in this crate starts from something the file under the cursor writes down:
//! a receiver, a constant, a `def`. A template writes none of them. `<%= time_ago(story) %>` is a
//! call on an **implicit receiver** whose class Rails builds at render time out of three things
//! no file names — every module under `app/helpers`, the `helper_method` proxies of the
//! controller the template's *path* implies, and ActionView's own helper modules — so rubydex
//! correctly resolves it to nothing.
//!
//! **A generated `module` holding the view context cannot work**, which is why this is a rung
//! and not a declaration. RBS has no way to say *self in this file is X*, so the module would
//! exist and nothing would reach it; the obvious repair of wrapping the template's Ruby in a
//! `class … end` is closed by construction, because [`erb::ruby_view`] replaces markup with one
//! space per byte so every offset rubydex records is the template's own. Declaring the helpers
//! half in RBS would also give a second *place* to every helper method in the project.
//!
//! So it is one table, consulted by [`locator::resolve_typed`](super::locator::resolve_typed) where every other rung is and by
//! `completion` at the same cursor. The two read one walk, exactly as the concern edge reads
//! `locator::extended_modules`: resolution takes the first answer, completion collects
//! all of them, and the gate deciding what the view context *is* is stated once, here.
//!
//! # The three halves
//!
//! **`app/helpers`** needs no macro read. Rails globs `**/*_helper.rb` under each `app/helpers`
//! directory and includes every module it finds, so the `def`s are already in the graph and what
//! is missing is only the edge — [`rails::is_helper`] and a list of names. This is the large half
//! by a wide margin of the two an application writes.
//!
//! **`helper_method`** is the machinery. `AbstractController::Helpers` writes `def current_user`
//! per call onto the controller's `_helpers` module, so what the macro hands over is a
//! *permission* rather than a member: the `def` it names is already on the controller.
//!
//! **ActionView's own**, [`rails::VIEW_CONTEXT`], is the half no application writes and the
//! largest of the three by call sites. `ActionView::Base` is built as
//! `include Helpers, ::ERB::Util, Context`, and `ActionView::Helpers` `include`s its 24 helper
//! modules at module-body level, so **one name reaches every one of them** and rubydex's
//! linearization — which already holds actionview, because it is in the bundle — does the walk.
//! Nothing is generated and nothing is declared: this half is a name to start an ancestor walk
//! from, and a project whose bundle has no actionview has no such declaration and so answers
//! nothing, which is the whole of the gate on it.
//!
//! The three are read in that order backwards — export, then `app/helpers`, then ActionView —
//! and the order is Ruby's own rather than a preference. The proxy sits **on** `_helpers`, an
//! application's module is `include`d into it, and ActionView's were included into the view
//! class before either, so `def tag` in `ApplicationHelper` really does shadow
//! `ActionView::Helpers::TagHelper#tag` for every template in the project.
//!
//! # What the table refuses
//!
//! - **A name in none of the three.** `can?`, `policy`, a decorator's method: the rung is
//!   additive, so those answer on the name rung exactly as they did. Of the six corpora's 8,732
//!   bare-word call sites 8,325 answer exactly and **407** are this residue, `can?`'s 151 the
//!   largest of them.
//! - **A controller method nobody exported.** `helper_method` is the whole of the gate on that
//!   half, per name and never per class: a template may call `current_user` because the class
//!   said so, and may not call `set_story` because it did not.
//! - **A mailer's views do not get the `app/helpers` half.** `include_all_helpers` is
//!   `ActionController::Base`'s default and `ActionMailer::Base` has no such thing — a mailer
//!   reaches an application helper only by writing `helper` itself. A mailer template gets its
//!   own exports, what it named, and ActionView's half, which every view context has.
//! - **A helper file does not get the export half.** A module under `app/helpers` is *in* the
//!   view context rather than merely read by it, so [`Views::reachable`] answers for one — but
//!   `helper_method` is a permission one controller grants and a helper module is included into
//!   every controller's context, so there is no class to name and none is picked.
//!
//! Three bounds are stated rather than discovered. **An engine's own files**:
//! [`rails::is_helper`] and [`erb::is_template`] are asked of the path under the cursor, so a
//! helper or a template inside an indexed engine gets the *application's* helper modules in
//! scope. The population is six files across six corpora, which is why it is a sentence here
//! and not the first time this module reads `environment`. **Partials**: `rails::controller_of` reads the
//! directory and not the file name, so `shared/_header.html.erb` names a `SharedController`
//! nothing defines, and a partial under such a directory gets the other two halves only.
//! **`include_all_helpers = false`**: an application that sets it gets only its matching helper,
//! and that config is out of reach for the same reason `database.yml` is.

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
/// Both are conventions rather than facts a file states, which is what puts this answer in the
/// *derived* tier beside the view↔renderer rung it sits next to: nothing in the template says
/// which class renders it, and nothing in it says that `app/helpers` is in scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InView {
    /// A `def` in a module under `app/helpers`, which Rails includes in every view context.
    Helper,
    /// A `helper_method` written by the class the template's path names, or by one of its
    /// ancestors. The name is the class Rails renders the template from.
    Exported(String),
    /// A `def` in one of the modules ActionView itself puts in every view context —
    /// [`rails::VIEW_CONTEXT`]. The outermost rung, so an application's own helper shadows it.
    Framework,
}

/// One member a bare word in a template reaches.
pub struct Reached {
    pub declaration: DeclarationId,
    /// How far out the module that installed it sits, on the chain Rails builds `_helpers` from.
    ///
    /// A `helper_method` proxy is written **on** `_helpers`, an application's helper module is
    /// `include`d into it, and ActionView's own were included into the view class before
    /// either — so each half is nearer than the next by exactly one step, which is also the
    /// order the three answer in when more than one holds the name, and Ruby's own answer for a
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
    /// Whether this project wants a view context at all — `[rails] views`.
    ///
    /// A flag and not an empty map, because the two halves of this module fail differently when
    /// they are empty: `named_by` answers nothing, which is harmless, while the **renderer**
    /// half asks `rails::controller_of` of a path and would go on citing a controller that does
    /// not exist. A Sinatra or Hanami application with an `app/views/` is precisely the project
    /// that has to be able to say no, and an empty map would not have said it.
    ///
    /// `Views::default()` is therefore **off**, which is also what the pass leaves in place when
    /// the switch says so.
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
    /// `None` for every document that is neither a template nor a helper, which is the first
    /// test and the cheap one: this is asked of every call in the project that rubydex could
    /// not resolve, and an ordinary `.rb` file must pay a path test and no more. `None` also
    /// where all three halves are empty, so that a Rails application with no helpers, no
    /// exports and no actionview in its bundle costs nothing further.
    ///
    /// **A module under `app/helpers` is *in* the view context, not merely read by it.** Rails
    /// includes every one of them into the same `_helpers`, so a bare call written in one
    /// reaches the other helper modules and ActionView's own exactly as a template's does — and
    /// a helper file is the only other place in an application where that is true. What it does
    /// **not** get is the export half: `helper_method` is a permission one controller grants,
    /// and a helper module is included into every controller's view context, so there is no
    /// class for [`rails::controller_of`] to name and no honest way to pick one. That refusal is
    /// why the renderer lookup below is inside the template arm.
    #[must_use]
    pub fn reachable(&self, graph: &Graph, uri_id: UriId) -> Option<Reachable> {
        if !self.enabled {
            return None;
        }
        let path = DocUri::from_uri_str(graph.documents().get(&uri_id)?.uri())?.to_path()?;
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

        // The framework's half, which every view context gets and no file asks for: a mailer's
        // template, a partial under `shared/` whose controller does not exist, and a helper
        // module all render through an `ActionView::Base`. Resolved rather than declared —
        // actionview is in the bundle and therefore in the graph already, so an application
        // without one answers nothing here and needs no switch to say so.
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
    /// **The order is Rails' own and not a preference.** `app/views/user_mailer/` names a
    /// `UserMailerController` that does not exist, so the mailer is reached exactly where the
    /// controller is not — and an application that *does* define a `UserMailerController` has
    /// said something this rule must not overrule.
    ///
    /// **The second half is gated and the first does not have to be.** [`rails::controller_of`]
    /// produces a name nothing but a controller is called; [`rails::mailer_of`] produces
    /// whatever the directory happens to spell — `app/views/shared/` spells `Shared` — so it
    /// answers only for a class this application defines that [`rails::is_mailer`] recognises.
    /// That gate is [`Views::mailers`](Views), which the pass fills from the superclasses it
    /// already holds.
    ///
    /// Public because this is the one convention two modules read, and they must not disagree:
    /// here it decides what a template may **call**, and in [`types`](super::types) it decides
    /// where a template's `@ivar` was **written** and which class a card names. A view context
    /// built from a mailer beside a card citing a controller would be two answers about one
    /// path.
    ///
    /// `None` for every path that is not a template, so a caller holding a path need not test
    /// that itself, and `None` for the whole of it when `[rails] views` is off.
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

/// The class a template's path names, resolved against the graph.
///
/// Handed out rather than kept private because two modules ask the same question of one path —
/// see [`Views::rendered_by`]. What each does with the answer is its own: here the declaration is
/// an ancestry to walk for what the template may call, and in [`types`](super::types) it is the
/// documents that class is written in, with the name as the `self` an `@story` has to be written
/// under to count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedBy {
    /// The class, fully qualified, as the path spells it.
    pub name: String,
    pub declaration: DeclarationId,
    /// Whether a controller answered rather than a mailer.
    ///
    /// The one thing the two spellings decide differently, and both readers need it: a view
    /// context gets the `app/helpers` glob only from a controller, and a card has to say which
    /// of the two conventions it followed — "the mailer Rails renders this template from" is a
    /// different sentence, and calling a mailer a controller would be a card that is wrong
    /// about the one fact it is citing.
    pub controller: bool,
}

/// The class a template's path names, and what it lets a template call.
struct Renderer {
    name: String,
    declaration: DeclarationId,
    /// Whether it is a controller rather than a mailer — [`RenderedBy::controller`], and here it
    /// is the `app/helpers` glob that turns on it.
    controller: bool,
    exported: BTreeSet<String>,
}

/// One template's view context, resolved against the graph.
pub struct Reachable {
    renderer: Option<Renderer>,
    helpers: Vec<DeclarationId>,
    /// The framework's own half: whichever of [`rails::VIEW_CONTEXT`] the graph holds.
    ///
    /// Empty for a project whose bundle has no actionview in it, which is the whole of the gate
    /// on this half — there is no switch and no path test, because a module that is not in the
    /// graph cannot be walked and a module that is could only have got there from the bundle.
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
        // Last, and that is the whole of what keeps this half additive rather than exclusive:
        // an application that writes `def tag` in `ApplicationHelper` has shadowed
        // `ActionView::Helpers::TagHelper#tag` for every one of its templates, and the two
        // lookups above have already answered by the time this one is asked.
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
        for (modules, step) in [(&self.helpers, HELPER), (&self.framework, FRAMEWORK)] {
            for module in modules {
                for (_, namespace) in ancestors_of(graph, *module) {
                    for (name, member) in namespace.members() {
                        // `include` installs methods and nothing else: a constant nested in a
                        // helper module is not reachable from a template through the view
                        // context.
                        if !matches!(
                            graph.declarations().get(member),
                            Some(Declaration::Method(_))
                        ) {
                            continue;
                        }
                        // **Visibility is deliberately not read here**, and the framework half
                        // is what made the question worth answering: ActionView marks 185 of
                        // its 438 helper `def`s private, so filtering would drop about a third
                        // of the rows this half adds. It is `completion`'s own rule — a cursor
                        // with no receiver written sets `private_ok`, because Ruby really does
                        // let an implicit receiver call a private method — and measuring the
                        // filter said the same thing twice: over 297 real template lists it
                        // *lost* 932 rows against 312, because the corpora's own helper modules
                        // mark 192 methods private and every one of them was already offered.
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
///
/// The order is Ruby's own: the proxy `helper_method` writes sits **on** `_helpers`, an
/// application's helper module is `include`d into it, and ActionView's modules were included
/// into the view class before either — so the framework's half is the furthest away and the one
/// an application shadows by writing its own `def` of the same name.
const EXPORTED: u16 = 0;
const HELPER: u16 = 1;
const FRAMEWORK: u16 = 2;

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

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;

    /// A controller, a helper and a template, in the layout the view context reads.
    ///
    /// The controller exports one of its two methods, deliberately: `helper_method` is a
    /// permission and the whole of the gate on that half, so a fixture with one method
    /// could not tell "this template may call it" from "this project defines it once".
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
    /// Written as workspace Ruby rather than pulled out of a bundle, because what this half
    /// needs from the gem is a *graph* and not a file: `ActionView::Helpers` `include`s its
    /// helper modules at module-body level, so rubydex linearizes them and one name reaches
    /// every one. The three `def`s are the three the tests below ask for, and `t` is an alias
    /// of `translate` because that is how the real `TranslationHelper` writes the commonest
    /// call in any Rails application.
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
    fn a_project_that_is_not_rails_can_say_so_and_the_view_context_goes_quiet() {
        // **The case `[rails] views` exists for**, and it is about a wrong answer rather than a
        // slow one: `rails::controller_of` reads a *path*, so any project with an `app/views/`
        // gets Rails' convention applied to it — a Sinatra or Hanami application included, where
        // the controller the card cites does not exist.
        //
        // Off, the same cursor falls to the rung below, which is the answer the workspace would
        // have given if nobody had written a controller at all.
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
        // project, so the name rung finds the same method either way — which is the point: the
        // difference a user sees is the *tier*, and with the view context off this is a guess
        // rather than a card reached through the class the path names.
        let mut harness = Harness::new();
        let view = view_context_app(&harness);
        harness.index();
        let cited = card(&mut harness, &view, source, "current_user");
        assert!(cited.contains("StoriesController#current_user"), "{cited}");
        assert!(!cited.contains("method name alone"), "{cited}");
    }

    #[test]
    fn a_helper_method_export_is_what_a_template_may_call() {
        // The view context, first clause. `current_user` in a template already jumps —
        // there is one method of that name in the project — and it jumped on the *name* rung,
        // which is the same answer it would give if the controller had never exported it. What
        // changes is the tier and the gate: the answer is now reached through the class the
        // path names, and the method the class did **not** export is not reachable at all.
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

        // The obvious half is the one a two-example probe can see; this is the other
        // one, and it is thirty times larger over the corpus.
        let offered = harness.declarations_at(&view, "<%= curr~ %>\n");
        assert!(offered.contains(&"current_user".to_owned()), "{offered:?}");

        // And the method nobody exported is not in the view context, in either request. It is
        // still *findable* — one `set_story` in the project, so the name rung answers — and the
        // card says so, which is the difference this gate exists to keep.
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
        // answer is a permission rather than a member. A splat and an interpolated symbol are
        // Ruby that only runs, and a bare `helper_method` is a no-op Rails accepts — so all
        // three export nothing, and `render_story` goes on answering what it answered before
        // rather than becoming callable because a call of the right name was written.
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
        // The other half, and it needs no macro at all: Rails' `all_helpers_from_path` globs
        // `app/helpers/**/*_helper.rb` and includes every module it finds in every view
        // context. So this template's controller does not exist — `app/views/comments/` names a
        // `CommentsController` this application has never written — and the helpers answer
        // anyway, which is what 489 of the corpus' 913 partials live on.
        let mut harness = Harness::new();
        view_context_app(&harness);
        harness.write(
            "app/helpers/stories_helper.rb",
            "module StoriesHelper\n  def byline\n  end\nend\n",
        );
        // Under `app/helpers` and not named the way Rails' glob names them: this is solidus'
        // `controller_helpers/auth.rb` shape, which is reached by an `include` a controller
        // writes and is in no view context by default.
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

        // Every module, not the one whose name matches the directory: `include_all_helpers` is
        // the Rails default, and an application that turns it off is a bound this states rather
        // than reads.
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
        // includes a module *into* it, so where both hold a name the proxy is what runs. A third
        // of real export sites name a method that is also an `app/helpers` `def`, so this is the
        // commonest shape the two halves have together and not an edge.
        //
        // **One name is one row.** The same pair in a completion list would be the same word
        // twice, one of which jumps somewhere the call would not go.
        //
        // **An export naming nothing declares nothing.** `helper_method :missing` is a
        // permission for a method that does not exist, so the half below it answers instead.
        //
        // **A constant in a helper module is not in the view context.** `include` installs
        // methods; `ApplicationHelper::MAX` is reached by writing it out.
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
        // Somewhere the view context cannot reach, so that the export declining is visible as
        // the rung below answering rather than as a hover with nothing on it.
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
        // 8 of the six corpora's 60 exported names are written in a concern and 7 more in a
        // module under `app/helpers` that a controller `include`s, so a reader that walked
        // `app/controllers` and keyed by class would find 24 of solidus' exports and reach 3 of
        // its 113 sites. Nothing here knows what a concern is: the export list is keyed by the
        // body that wrote the macro, and the walk from the template's class up its own
        // ancestors is what crosses the `include` the controller already wrote.
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
        // The class the *template* names, not the module the macro is in: what a reader has to
        // be able to check is that Rails renders this template from there.
        assert!(
            through.contains("`helper_method` in `StoriesController`"),
            "{through}"
        );
    }

    #[test]
    fn a_mailer_gets_its_own_exports_and_not_the_applications_helpers() {
        // `AbstractController::Helpers` is in `ActionMailer::Base` too, so `helper_method` in a
        // mailer is real — 3 of the corpus' 60 — and the view path a mailer renders from is its
        // own name with no `Controller` on the end. `include_all_helpers` is **not**: it is
        // `ActionController::Base`'s default, and a mailer reaches an application helper only
        // by writing `helper` itself, which is a call this reader does not read. So the two
        // halves separate here and nowhere else.
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

        // And the way back in is the one Rails gives: `helper :application` names the module,
        // and a mailer that names it has said the only thing that puts an application helper in
        // front of a mailer template. Most real `helper` calls are in a mailer, and they are the
        // whole of the gap.
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

        // And the directory has to name a **mailer**: `mailer_of` spells whatever the path
        // spells, so the gate is the application's own superclass table and not the path.
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
        // Two refusals in one fixture, and both are safety rather than feature.
        //
        // **Additive.** The view context is three halves and the order between them is Ruby's:
        // lobsters writes `def tag` in `ApplicationHelper` deliberately, to shadow
        // `ActionView::Helpers::TagHelper#tag`, so a template's `tag` must reach the
        // application's — while `link_to`, which the application does not redefine, reaches
        // ActionView's. The application module is `include`d into `_helpers` and ActionView's
        // were included into the view class before it, so "nearer wins" is the whole rule.
        //
        // **A template and nothing else.** The same bare call in a `.rb` file that is not a
        // helper is an ordinary receiverless call whose `self` is `main`, and Rails puts no
        // helpers on that.
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
        // The rung is additive at its own edge too, and this is the assertion that says so:
        // `can?` is cancan's, `policy` is pundit's, and neither is in any half. 407 of the six
        // corpora's 8,732 bare-word call sites are this residue, `can?`'s 151 the largest, and
        // every one must go on answering what it answered before this half existed.
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
        // The whole of the gate on the third half, and it is deliberately not a switch: the
        // half is two *names*, and a name the graph does not hold cannot be walked. So a
        // project with an `app/views/` and no Rails in its bundle — the Sinatra application
        // `[rails] views` exists for, and equally a Rails application whose gems are not
        // installed yet — gets the two halves it has evidence for and the name rung under them.
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
        // 34 of the 121 positions this half was opened for are in an `app/helpers` file rather
        // than in a template, and they are the same defect: Rails includes every helper module
        // into the same `_helpers`, so a bare call written in one reaches the other helper
        // modules and ActionView's own exactly as a template's does.
        //
        // **What it does not get is the export half.** `helper_method` is a permission one
        // controller grants; a helper module is included into every controller's view context,
        // so there is no class for `rails::controller_of` to name and none is picked — which is
        // also the one thing that would be wrong if this lane simply reused the template's.
        //
        // **And Rails' own glob still decides.** `app/helpers/legacy/auth.rb` is not named
        // `*_helper.rb`, so it is in no view context and nothing about it changes.
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
        // The half that is every view context's, stated against the one template that has the
        // least: `include_all_helpers` is `ActionController::Base`'s and `ActionMailer::Base`
        // has no such thing, so a mailer reaches an application helper only by naming it — but
        // every renderer builds an `ActionView::Base`, so `link_to` and `t` work in a mailer
        // view and always have. A partial under `shared/`, whose controller does not exist at
        // all, is the same shape and is why this half reaches the residue the other two leave.
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
        // `ActionView::Base` is `include Helpers, ::ERB::Util, Context`, and the middle one is
        // the smallest of the three by a wide margin: two words over six corpora, `h` and
        // `json_escape`, at 38 call sites. It is here because `h` is a template idiom and the
        // row costs one name. `Context` is not, and the reason is the same measurement read the
        // other way — `output_buffer` and `view_flow` are the renderer's plumbing and no
        // template in six corpora calls either.
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
        // `_helpers` from — export 0, `app/helpers` 1, ActionView 2 — so a template offers the
        // word the application itself wrote above the one the framework ships, which is the
        // order the two would run in.
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
