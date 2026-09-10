//! `config/routes.rb`: which helpers Rails names, and the line of the DSL that named each.
//!
//! `*_path` and `*_url` call sites are everywhere in a Rails application and in its templates,
//! and none of them resolves without this reader. All of it is static and every helper returns
//! `String`, so `story_path` jumps to the `resources :stories` that named it.
//!
//! # The naming rule is Rails', copied rather than approximated
//!
//! `ActionDispatch::Routing::Mapper#name_for_action` joins four words and the *scope level*
//! decides their order:
//!
//! | level | the join |
//! | --- | --- |
//! | `:collection` | `[prefix, names, collection_name]` |
//! | `:member` | `[prefix, names, member_name]` |
//! | `:new` | `[prefix, "new", names, member_name]` |
//! | `:nested` | `[names, prefix]` |
//! | `:root` | `[names, collection_name, prefix]` |
//! | anything else | `[names, member_name, prefix]` |
//!
//! `names` is the prefix `namespace`, `scope as:` and nesting have accumulated; `prefix` is the
//! route's `as:` or its action, dropped when the action is one of [`CANONICAL`] at a level that
//! has a resource. That table is the whole feature: nothing here is a rule of thumb about how
//! Rails spells things.
//!
//! The reader is checked against **the real router** rather than against fixtures —
//! `ActionDispatch::Routing::RouteSet` draws each corpus' own routes files and its
//! `named_routes.names` is compared with this reader's, file by file. What it does not name is
//! one construct: a path interpolated from a loop variable, where Rails names a helper per
//! iteration and the text says none of them.
//!
//! # Three things this reads that look like Ruby running and are not
//!
//! - **`if`, `unless` and their `else`.** Both arms are routes written in the file with exact
//!   names and exact spans; what the condition decides is whether they are *mounted*, which no
//!   reader of text can know — and which is equally true of the `Rails.env.development?` blocks
//!   that are live in the environment an editor runs in. Applications routinely put a whole
//!   front end or a whole engine inside one.
//! - **A literal array with a block.** `%w[a b].each do |x| … end` is walked **once**: every name
//!   inside it that is a literal is the same on every pass, and every name that is not declines
//!   for being interpolated.
//! - **An unknown call that carries a block.** `constraints`, `defaults`, `authenticate`,
//!   `devise_scope :super_admin do … end` — every one changes what a route *requires*, never what
//!   it is called, so the block is walked in the scope it was written in. There is deliberately
//!   no allowlist: the four Rails ships and the two Devise does would be a list that is wrong for
//!   the seventh, and the real router does the same thing for the same reason.

use std::collections::BTreeSet;

use ruby_prism::{ArgumentsNode, CallNode, Node, StatementsNode};

use super::entrypoints::{Convention, convention_of};
use super::inflect::singularize;
use super::syntax::{constant_spelling, keyword, string_literal, symbol_or_string};
use super::{CONTROLLERS, ROUTE_HELPERS};
use crate::generated::{Declared, Facts, Owner, Source};

/// The verbs that define a route, and `match`, which is all of them at once.
const VERBS: [&str; 8] = [
    "get", "post", "put", "patch", "delete", "options", "head", "match",
];

/// `Mapper::Resources::CANONICAL_ACTIONS`, and it is a list about *names* rather than actions.
///
/// An action on this list contributes no word of its own at a level that has a resource, which
/// is why `resources :stories` names `story_path` and not `show_story_path`. At any other level
/// the same word is an ordinary prefix: a top-level `get "new"` is `new_path`.
const CANONICAL: [&str; 6] = ["index", "create", "new", "show", "update", "destroy"];

/// Calls that name a route this reader will not name, and each is a decline with a reason.
///
/// `mount` really does install a helper — `mount Sidekiq::Web, at: "/sidekiq"` is
/// `sidekiq_web_path` — and it is declined because the name comes from **a constant's**, which
/// is a second inflector over somebody else's class name for the corpus' 23 calls. `direct` and
/// `resolve` name a helper from a block that only runs. `devise_for` installs about fifteen
/// whose list depends on which Devise modules the model declares, which is a file this does not
/// read: it is the largest single miss in the corpus — forem's `sign_up_path` alone is 129 call
/// sites — and it is a miss rather than a guess.
const DECLINED: [&str; 5] = [
    "mount",
    "direct",
    "resolve",
    "devise_for",
    "mount_devise_token_auth_for",
];

/// Where in a resource the router is, which is the only thing that decides how a name joins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Level {
    /// The top of the file, or any scope that is not a resource's.
    #[default]
    Top,
    /// Directly inside `resources :stories do … end`.
    Resources,
    /// Directly inside `resource :session do … end`.
    Resource,
    /// One resource written inside another's block, and the level `nested` puts it at.
    Nested,
    /// `member do … end`, and every route a resource's `:show`, `:edit`, `:update` implies.
    Member,
    /// `collection do … end`, and `:index`.
    Collection,
    /// `new do … end`, and `:new`.
    New,
    /// `root` written inside a `resources` block.
    Root,
}

impl Level {
    /// Whether a resource or a `namespace` written here is `nested` — `RESOURCE_SCOPES`.
    fn nests(self) -> bool {
        matches!(self, Self::Resources | Self::Resource)
    }

    /// Whether [`CANONICAL`] applies here — `RESOURCE_METHOD_SCOPES`.
    fn canonical(self) -> bool {
        matches!(self, Self::Member | Self::Collection | Self::New)
    }
}

/// Everything about where a route is written that changes what it is called.
///
/// A value rather than a stack, so that entering a block is `walk(body, &scope.with(…))` and
/// leaving it is returning: nothing has to be undone, which is the bug `with_scope_level`'s
/// `ensure` exists to prevent in the original.
#[derive(Debug, Clone, Default)]
struct Scope {
    /// `@scope[:as]`: the words `namespace`, `scope as:` and nesting have accumulated.
    names: Vec<String>,
    /// `@scope[:shallow_prefix]`: the same, minus the words nesting contributed.
    shallow_names: Vec<String>,
    /// Whether a member route here drops the nesting — `shallow: true` or a `shallow do` block.
    shallow: bool,
    level: Level,
    /// `(collection_name, member_name)` of the resource this is inside, if any.
    resource: Option<(String, String)>,
}

impl Scope {
    /// `Mapper::Scope#action_name`, joined and cleaned: the name of one route.
    ///
    /// Empty for a route Rails leaves unnamed, and the two ways that happens are both here: no
    /// word survives the join, or the first character is not one a method name may start with.
    fn named(&self, prefix: Option<&str>) -> String {
        let (collection, member) = match &self.resource {
            Some((collection, member)) => (Some(collection.as_str()), Some(member.as_str())),
            None => (None, None),
        };
        let names = self.names.join("_");
        let names = (!names.is_empty()).then_some(names.as_str());
        let parts: [Option<&str>; 4] = match self.level {
            Level::Nested => [names, prefix, None, None],
            Level::Collection => [prefix, names, collection, None],
            Level::New => [prefix, Some("new"), names, member],
            Level::Member => [prefix, names, member, None],
            Level::Root => [names, collection, prefix, None],
            Level::Top | Level::Resources | Level::Resource => [names, member, prefix, None],
        };
        let name = parts
            .into_iter()
            .flatten()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_");
        if name.starts_with(|first: char| first == '_' || first.is_ascii_alphabetic()) {
            name
        } else {
            String::new()
        }
    }

    /// `nested`: the parent's member name joins the prefix, and this stops being a resource
    /// scope so that it happens once however deep the nesting goes.
    fn nested(&self) -> Self {
        let Some((_, member)) = self.resource.as_ref().filter(|_| self.level.nests()) else {
            return self.clone();
        };
        let mut inner = self.clone();
        inner.names.push(member.clone());
        inner.level = Level::Nested;
        inner
    }

    /// The same scope at another level, which is what `member`, `collection` and `new` are.
    fn at(&self, level: Level) -> Self {
        Self {
            level,
            ..self.clone()
        }
    }

    /// A word added to the prefix by `namespace :admin` or `scope as: "admin"`.
    fn prefixed(&self, word: &str) -> Self {
        let mut inner = self.clone();
        inner.names.push(word.to_owned());
        inner.shallow_names.push(word.to_owned());
        inner
    }

    /// The scope a member route of a shallow resource is named in: the nesting is dropped and
    /// the words `namespace` contributed are kept.
    fn shallowed(&self) -> Self {
        if !self.shallow {
            return self.clone();
        }
        let mut inner = self.clone();
        inner.names = self.shallow_names.clone();
        inner
    }
}

/// One helper Rails would name, and the DSL call that named it.
#[derive(Debug, Clone)]
struct Helper {
    /// The name without its `_path` / `_url` suffix, which is how Rails holds it too.
    name: String,
    /// The whole call — `resources :stories, only: [:index]`.
    at: (u32, u32),
    /// What to select inside it: the symbol, the path, or the `as:`.
    name_at: (u32, u32),
    /// The call as it should be quoted back to a reader: `resources :stories`.
    spelled: String,
}

/// A `draw :admin`, and the prefix the scope it was written in had accumulated.
///
/// The reader cannot open `config/routes/admin.rb` itself — nothing in this directory does I/O —
/// so it says which file and at which prefix, and the caller reads it. That the prefix travels
/// is not a nicety: forem draws `config/routes/api.rb` **twice**, once under `scope module: :v1`
/// and once under `:v0`, and mastodon draws all five of its at the top level.
#[derive(Debug, Clone)]
pub struct Draw {
    pub name: String,
    pub prefix: Vec<String>,
}

/// Every helper one routes file names, and every file it draws.
#[derive(Debug, Default)]
pub struct Routes {
    helpers: Vec<Helper>,
    draws: Vec<Draw>,
}

/// Read one routes file, at the prefix the `draw` that reached it had accumulated.
///
/// `prefix` is empty for `config/routes.rb` itself and is the scope's words for a drawn file.
#[must_use]
/// Whose routes file this is, which is the whole of what makes a `draw` receiver mean anything.
///
/// The application's own file may draw into any route set and every helper it names is one this
/// project's classes call: solidus writes `Spree::Core::Engine.routes.draw` in its own
/// `core/config/routes.rb` for its own routes, and reading receiver-blind is wrong on
/// purpose because of it. **A gem's file is the case where the receiver carries information.**
/// Four of the seven engines that ship a routes file open with `Rails.application.routes.draw`,
/// so their helpers land on the host application's controllers exactly as if the application had
/// written them; blazer, pghero and mission_control-jobs draw into their own `Engine.routes`,
/// and those helpers are reached as `blazer.queries_path` after a `mount` — a spelling this
/// crate does not read and which zero of six applications use.
///
/// So the discriminator is receiver **and** location, not receiver alone, which is why this is a
/// parameter rather than a rule inside the reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Whose {
    /// The workspace's own routes file. Any `.routes.draw` receiver, and no wrapper at all is
    /// fine too — a file reached by `draw :admin` is its body directly.
    Own,
    /// A gem's. Only `Rails.application.routes.draw` reaches the host application, and anything
    /// else — including a file with no wrapper — declares **nothing** rather than falling
    /// through to the top-level statements.
    Gem,
}

pub fn read_routes(source: &str, prefix: &[String], whose: Whose) -> Routes {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        named: BTreeSet::new(),
        routes: Routes::default(),
        concerns: Vec::new(),
    };
    let scope = Scope {
        names: prefix.to_vec(),
        shallow_names: prefix.to_vec(),
        ..Scope::default()
    };
    // `Rails.application.routes.draw do … end` wraps the whole file, and an engine's spelling —
    // `Spree::Core::Engine.routes.draw do` — is the same call on a different receiver. A drawn
    // file has no wrapper at all and its statements are the body directly, which is why the
    // wrapper is looked for rather than required.
    //
    // For a gem it is required *and* its receiver is read, and the fall-through is deliberately
    // not taken: `Reader::call` walks an unknown call's block transparently, so falling back to
    // the top-level statements would read `Blazer::Engine.routes.draw`'s body as though it were
    // the application's. `None` walks nothing.
    let statements = parsed
        .node()
        .as_program_node()
        .map(|program| program.statements());
    let wrapper = statements.as_ref().and_then(wrapper);
    let body = wrapper
        .as_ref()
        .and_then(|call| call.block()?.as_block_node()?.body()?.as_statements_node());
    let body = match whose {
        Whose::Own => body.or(statements),
        Whose::Gem => body.filter(|_| {
            wrapper.as_ref().is_some_and(|call| {
                call.receiver().is_some_and(|receiver| {
                    constant_spelling(source, &receiver) == APPLICATION_ROUTES
                })
            })
        }),
    };
    reader.walk(body.as_ref(), &scope, &mut Vec::new());
    reader.routes
}

/// Whether the route helpers are `include`d into this class or module.
///
/// Three rules and each covers what the others cannot. The framework's two base classes are
/// exact, because they are the hook Rails itself hangs on. The `Controller` suffix is what
/// reaches an application whose base is a **gem's** class — every application built on solidus
/// or spree, which is `Spree::StoreController < ActionController::Base` inside a gem this
/// application only depends on — and it is the same suffix rule a `Mailer` gets.
/// And a mailer is [`convention_of`]'s already, asked here so that the two cannot disagree about
/// what a mailer is.
///
/// A **module** is a host when its name ends `Helper`, which is Rails' own convention for
/// `app/helpers` and the only thing about a helper module that a name can see. The path would be
/// a better test and is not available: a generated document is keyed by the source file, but
/// which file a *class* was defined in is not something the pass records, and recording it for
/// one suffix would be a projection of the graph for a rule the corpus does not stress —
/// **45 call sites in six applications** are inside a helper module, against 868 in a controller.
///
/// **A name that is not a constant path is not a host, and that clause is not defensive.** The
/// name here is the one rubydex holds, and for an anonymous class — `Class.new(ApplicationController)`,
/// which forem writes in its specs — that is `1640350138339398774:1364<anonymous>`. Three of them
/// reached this in forem, `class` + that is not RBS, and
/// [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record)'s parse gate then
/// threw away **the whole document** — which is the one carrying every `include`, so the feature
/// was silently reduced to the name rung across the entire application. It is also right on its
/// own terms: an anonymous class has no name to reopen, so a declaration on it could never reach
/// anything. The test itself is [`generated::is_constant_path`](crate::generated::is_constant_path)
/// rather than a copy of it here, because the same shape is reachable a second time from a
/// different generator and one rule found twice is one rule.
#[must_use]
pub fn hosts_routes(name: &str, superclass: Option<&str>, mixins: &[String], module: bool) -> bool {
    if !crate::generated::is_constant_path(name) {
        return false;
    }
    if module {
        return name.ends_with("Helper");
    }
    if name.ends_with("Controller") {
        return true;
    }
    match superclass {
        Some(superclass) if CONTROLLERS.contains(&superclass) => true,
        superclass => matches!(convention_of(superclass, mixins), Some(Convention::Mailer)),
    }
}

/// Every class and module the route helpers are `include`d into, as one generated document.
///
/// Rails installs them with an `inherited` hook on `ActionController::Base` and
/// `ActionMailer::Base`, so **every** controller and mailer really does get its own copy — which
/// is why this writes one `include` per host rather than one on a base and a hope that the base
/// is the application's. An application whose controllers descend from a gem's class, which is
/// every application built on solidus or spree, has no base this pass defines — and writing
/// every host bounds that gap at zero.
#[must_use]
pub fn mixins(hosts: &BTreeSet<Owner>) -> Facts {
    let mut facts = Facts::default();
    for host in hosts {
        facts.mixin(host.clone(), ROUTE_HELPERS.to_owned());
    }
    facts
}

impl Routes {
    /// Every file this one draws, and the prefix each was drawn at.
    pub fn draws(&self) -> &[Draw] {
        &self.draws
    }

    /// Every helper this file names, so the caller can decide which file writes which.
    ///
    /// Two routes files naming one helper is ordinary — an engine and its host application, or
    /// the same name in `config/routes.rb` and a drawn file — and the two declarations would land
    /// in two *different* generated documents, where [`Facts`]' precedence cannot see them and
    /// RBS would hold them as an overload set. The caller resolves it the way a shared relation
    /// class is resolved: first in URI order writes it.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.helpers.iter().map(|helper| helper.name.as_str())
    }

    /// The RBS this file's routes declare, for the helpers `emit` says are this file's.
    #[must_use]
    pub fn signatures(&self, file: &str, emit: &BTreeSet<String>) -> Facts {
        let mut facts = Facts::default();
        for helper in &self.helpers {
            if !emit.contains(&helper.name) {
                continue;
            }
            for suffix in ["_path", "_url"] {
                facts.declare(Declared {
                    owner: Owner::Module(ROUTE_HELPERS.to_owned()),
                    name: format!("{}{suffix}", helper.name),
                    returns: "String".to_owned(),
                    parameters: "(*untyped)".to_owned(),
                    because: format!("From `{file}`, `{}`.", helper.spelled),
                    at: Some((helper.at, helper.name_at)),
                    from: Source::Convention,
                    overloads: Vec::new(),
                });
            }
        }
        if !facts.is_empty() {
            facts.note(
                Owner::Module(ROUTE_HELPERS.to_owned()),
                "Rails' route helpers. Rails' own module is anonymous; this one is ya-lsp's, and \
                 every method in it points at the line of the routing DSL that named it."
                    .to_owned(),
            );
        }
        facts
    }
}

/// The receiver of the only `draw` in a gem that reaches the host application.
///
/// `constant_spelling` drops a leading `::`, so `::Rails.application.routes.draw` counts; a
/// receiver split across lines does not, and declining is the safe direction — a helper not
/// offered, rather than one invented on every controller in the project.
const APPLICATION_ROUTES: &str = "Rails.application.routes";

/// The `… .routes.draw do` this file opens with, wherever in the program it is.
///
/// **Not simply a statement of the program**, and the corpus is emphatic about why: both
/// `activestorage` and `turbo-rails` end their routes file `end if ActiveStorage.draw_routes`, a
/// modifier `if`, so the wrapper is an `IfNode`'s body. That is two of the four engines which
/// draw into the application and **ten of the twenty-one helpers** they name between them —
/// including every one of activestorage's, which is where the corpus' single call site is.
///
/// It was a latent defect on the workspace side too, not a new rule for gems: [`Reader::walk`]
/// skips a call that has a receiver, so falling back to the top-level statements never found a
/// wrapped `draw` either. Descending here fixes both at once.
fn wrapper<'pr>(statements: &StatementsNode<'pr>) -> Option<CallNode<'pr>> {
    statements.body().iter().find_map(|statement| {
        if let Some(call) = statement.as_call_node() {
            return (call.name().as_slice() == b"draw" && call.block().is_some()).then_some(call);
        }
        let nested = statement
            .as_if_node()
            .and_then(|branch| branch.statements())
            .or_else(|| {
                statement
                    .as_unless_node()
                    .and_then(|branch| branch.statements())
            })?;
        wrapper(&nested)
    })
}

struct Reader<'src, 'pr> {
    source: &'src str,
    /// Every name already taken, because `has_named_route?` means the **first** route to claim a
    /// name keeps it and every later one is silently unnamed.
    named: BTreeSet<String>,
    routes: Routes,
    /// `concern :commentable do … end`, kept until a `concerns:` asks for it.
    concerns: Vec<(String, StatementsNode<'pr>)>,
}

impl<'pr> Reader<'_, 'pr> {
    /// One body of the DSL, and every statement of it that can hold a route.
    ///
    /// `None` is a body that is not there — a `do … end` this call does not have, an `else` with
    /// nothing in it — and is the ordinary case rather than a failure, which is why it is an
    /// argument here instead of a test at every call site.
    fn walk(
        &mut self,
        statements: Option<&StatementsNode<'pr>>,
        scope: &Scope,
        hosts: &mut Vec<CallNode<'pr>>,
    ) {
        let Some(statements) = statements else {
            return;
        };
        for statement in statements.body().iter() {
            if let Some(call) = statement.as_call_node() {
                match call.receiver() {
                    None => self.call(call, scope, hosts),
                    // A literal list iterated with a block, walked **once**. Every name inside
                    // it that is a literal is the same on every pass, and every name that is
                    // interpolated from the block's parameter declines for not being one.
                    Some(receiver) if receiver.as_array_node().is_some() => {
                        self.walk(block_body(&call).as_ref(), scope, hosts);
                    }
                    Some(_) => {}
                }
            } else if let Some(branch) = statement.as_if_node() {
                self.branch(branch.statements(), branch.subsequent(), scope, hosts);
            } else if let Some(branch) = statement.as_unless_node() {
                self.branch(
                    branch.statements(),
                    branch.else_clause().map(|clause| clause.as_node()),
                    scope,
                    hosts,
                );
            }
        }
    }

    /// Both arms of a conditional, because both are routes and the condition is about mounting.
    fn branch(
        &mut self,
        taken: Option<StatementsNode<'pr>>,
        otherwise: Option<Node<'pr>>,
        scope: &Scope,
        hosts: &mut Vec<CallNode<'pr>>,
    ) {
        self.walk(taken.as_ref(), scope, hosts);
        let Some(otherwise) = otherwise else {
            return;
        };
        // `elsif` is an `IfNode` of its own and `else` is an `ElseNode`; both hold statements and
        // this reader wants them on the same terms.
        self.walk(
            otherwise
                .as_else_node()
                .and_then(|clause| clause.statements())
                .or_else(|| {
                    otherwise
                        .as_if_node()
                        .and_then(|branch| branch.statements())
                })
                .as_ref(),
            scope,
            hosts,
        );
        if let Some(branch) = otherwise.as_if_node() {
            self.branch(None, branch.subsequent(), scope, hosts);
        }
    }

    /// One call of the DSL.
    fn call(&mut self, node: CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        match name.as_str() {
            "resources" => self.resources(&node, scope, hosts, false),
            "resource" => self.resources(&node, scope, hosts, true),
            "namespace" => self.namespace(&node, scope, hosts),
            "scope" => self.scoped(&node, scope, hosts),
            "member" => self.block(&node, &scope.at(Level::Member), hosts),
            "collection" => self.block(&node, &scope.at(Level::Collection), hosts),
            "new" => self.block(&node, &scope.at(Level::New), hosts),
            "shallow" => self.block(
                &node,
                &Scope {
                    shallow: true,
                    ..scope.clone()
                },
                hosts,
            ),
            "with_options" => {
                // The body is taken before the node moves onto the stack; it borrows the parse
                // and not the node, so it outlives the move.
                let body = block_body(&node);
                hosts.push(node);
                self.walk(body.as_ref(), scope, hosts);
                hosts.pop();
            }
            "concern" => {
                if let (Some((named, _)), Some(body)) = (
                    first_symbol_or_string(self.source, &node),
                    block_body(&node),
                ) {
                    self.concerns.push((named, body));
                }
            }
            "concerns" => self.concerns(&node, scope, hosts),
            "root" => self.root(&node, scope, hosts),
            "draw" => {
                if let Some((named, _)) = first_symbol_or_string(self.source, &node) {
                    self.routes.draws.push(Draw {
                        name: named,
                        prefix: scope.names.clone(),
                    });
                }
            }
            _ if VERBS.contains(&name.as_str()) => self.verb(&node, scope, hosts),
            _ if DECLINED.contains(&name.as_str()) => {}
            // Everything else with a block is a wrapper until proven otherwise, which is the
            // rule the real router follows for the same reason: an unknown call that yields is
            // changing what a route requires, not what it is called.
            _ => self.block(&node, scope, hosts),
        }
    }

    /// A block walked in `scope`, if this call has one.
    fn block(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        self.walk(block_body(node).as_ref(), scope, hosts);
    }

    /// `resources :stories` and `resource :session`, and everything written in their block.
    fn resources(
        &mut self,
        node: &CallNode<'pr>,
        scope: &Scope,
        hosts: &mut Vec<CallNode<'pr>>,
        singular: bool,
    ) {
        let Some(arguments) = node.arguments() else {
            return;
        };
        let (spelled, at) = self.spelling(node, &arguments);
        // One call may name several — `resources :photos, :videos` — and Rails re-enters itself
        // once per name, so each gets the same options and its own span.
        for (given, name_at) in self.symbols(&arguments) {
            // `Resource#name` is `@as || @name`: `as:` replaces the word rather than decorating
            // it, so `resources :mails, as: "mod_mails"` is a resource called `mod_mails`.
            let named = self
                .inherited_text(node, hosts, "as")
                .map_or(given, |(as_, _)| as_);
            let (collection, member) = if singular {
                (named.clone(), named)
            } else {
                let member = singularize(&named);
                // Rails appends `_index` when a word is its own plural, so `resources :series`
                // names `series_index_path` and `series_path`.
                let collection = if member == named {
                    format!("{named}_index")
                } else {
                    named
                };
                (collection, member)
            };
            let outer = scope.nested();
            let shallow = self
                .inherited(node, hosts, "shallow")
                .map_or(scope.shallow, |value| value.as_true_node().is_some());
            let here = Scope {
                resource: Some((collection, member)),
                ..outer
            };
            let actions = self.actions(node, hosts, singular);
            let mut say = |scope: &Scope, prefix: Option<&str>| {
                self.declare(scope.named(prefix), at, name_at, &spelled);
            };
            let shallowed = Scope {
                shallow,
                ..here.clone()
            }
            .shallowed();
            if !singular && actions.iter().any(|it| ["index", "create"].contains(it)) {
                say(&here.at(Level::Collection), None);
            }
            if singular
                && actions
                    .iter()
                    .any(|it| ["show", "create", "update", "destroy"].contains(it))
            {
                say(&shallowed.at(Level::Member), None);
            }
            // `new` is a level of its own and is never shallow — which is why a shallow nested
            // resource still names `new_option_type_option_value_path`.
            if actions.contains(&"new") {
                say(&here.at(Level::New), None);
            }
            if actions.contains(&"edit") {
                say(&shallowed.at(Level::Member), Some("edit"));
            }
            if !singular
                && actions
                    .iter()
                    .any(|it| ["show", "update", "destroy"].contains(it))
            {
                say(&shallowed.at(Level::Member), None);
            }
            let inside = Scope {
                shallow,
                level: if singular {
                    Level::Resource
                } else {
                    Level::Resources
                },
                ..here
            };
            let wanted = self
                .inherited(node, hosts, "concerns")
                .map(|value| self.list(&value))
                .unwrap_or_default();
            self.expand(&wanted, &inside, hosts);
            self.block(node, &inside, hosts);
        }
    }

    /// `namespace :admin do … end`.
    ///
    /// `Resources#namespace` is `nested { super }` inside a resource scope, which is the one
    /// ordering rule that is easy to get backwards: `resources :accounts do namespace :whatsapp`
    /// names `account_whatsapp_calls`, because the parent's member name joins the prefix
    /// **before** the namespace's own word.
    fn namespace(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let Some((given, _)) = first_symbol_or_string(self.source, node) else {
            return;
        };
        let named = self
            .inherited_text(node, hosts, "as")
            .map_or(given, |(as_, _)| as_);
        self.block(node, &scope.nested().prefixed(&named), hosts);
    }

    /// `scope module: :admin do … end`, which changes a name only when it says `as:`.
    ///
    /// Unlike `namespace` this never nests: Rails' `scope` is `@scope.new(…)` with no
    /// `with_scope_level`, so a `resources` written inside one is still directly inside whatever
    /// resource the `scope` is in and nests there instead.
    fn scoped(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let mut inner = match self.inherited_text(node, hosts, "as") {
            Some((named, _)) => scope.prefixed(&named),
            None => scope.clone(),
        };
        if let Some(value) = keyword(node, "shallow") {
            inner.shallow = value.as_true_node().is_some();
        }
        self.block(node, &inner, hosts);
    }

    /// `concerns :commentable`, and `concerns: [:commentable]` on a resource.
    fn concerns(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let Some(arguments) = node.arguments() else {
            return;
        };
        let wanted: Vec<String> = arguments
            .arguments()
            .iter()
            .flat_map(|argument| self.list(&argument))
            .collect();
        self.expand(&wanted, scope, hosts);
    }

    /// `root to: "home#index"`, whose name is `root` unless it says otherwise.
    ///
    /// It really can say otherwise: `root` is `match "/", as: :root, via: :get, **options`, and
    /// because the caller's options come last a `root to: …, as: "categories_index"` overrides
    /// the word outright. discourse writes six of them.
    fn root(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let Some(arguments) = node.arguments() else {
            return;
        };
        let (spelled, at) = self.spelling(node, &arguments);
        let (named, name_at) = self
            .inherited_text(node, hosts, "as")
            .unwrap_or_else(|| ("root".to_owned(), at));
        let level = if scope.level == Level::Resources {
            Level::Root
        } else {
            Level::Top
        };
        self.declare(scope.at(level).named(Some(&named)), at, name_at, &spelled);
    }

    /// `get "about"`, `get :upvote, on: :member`, `post "/x" => "y#z", as: "w"`.
    fn verb(&mut self, node: &CallNode<'pr>, scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        let Some(arguments) = node.arguments() else {
            return;
        };
        let (spelled, at) = self.spelling(node, &arguments);
        // `decomposed_match`: a route written directly inside a resource's block and given no
        // `on:` is nested for a collection resource and a member route for a singular one.
        let on = self.inherited_text(node, hosts, "on");
        let here = match on.as_ref().map(|(word, _)| word.as_str()) {
            Some("member") => scope.at(Level::Member),
            Some("collection") => scope.at(Level::Collection),
            Some("new") => scope.at(Level::New),
            _ => match scope.level {
                Level::Resources => scope.nested(),
                Level::Resource => scope.at(Level::Member),
                _ => scope.clone(),
            },
        };
        if let Some((named, name_at)) = self.inherited_text(node, hosts, "as") {
            self.declare(here.named(Some(&named)), at, name_at, &spelled);
            return;
        }
        let Some((action, name_at)) = self.action(&arguments) else {
            // A path Rails cannot turn into an action still names a route — but only outside a
            // resource scope, where `name_for_action` returns `nil` outright. `namespace :mod`
            // with a `get "notes(/:period)"` in it is how a bare `mod_path` comes to exist.
            if here.resource.is_none() {
                self.declare(here.named(None), at, at, &spelled);
            }
            return;
        };
        let prefix =
            (!(here.level.canonical() && CANONICAL.contains(&action.as_str()))).then_some(action);
        self.declare(here.named(prefix.as_deref()), at, name_at, &spelled);
    }

    /// The word a verb's path or symbol contributes, by `Mapper.normalize_name`.
    ///
    /// `None` for everything Rails also refuses: no literal argument at all, an interpolated
    /// path, and a path holding a dynamic segment, a format or an optional group.
    fn action(&mut self, arguments: &ArgumentsNode<'pr>) -> Option<(String, (u32, u32))> {
        let first = arguments.arguments().iter().next()?;
        // `get :upvote` names the action outright; a path is normalized; `get "x" => "y#z"` puts
        // the path in the first *string* key of the trailing hash, which is `match`'s own rule.
        let (raw, at) = match symbol_or_string(self.source, &first) {
            Some(found) if first.as_symbol_node().is_some() => return Some(found),
            Some(found) => found,
            None if first.as_keyword_hash_node().is_some() => self.string_key(arguments)?,
            None => return None,
        };
        let normalized = normalize_name(&raw)?;
        Some((normalized, at))
    }

    /// The first string key of a call's trailing hash — the path of `get "x" => "y#z"`.
    fn string_key(&self, arguments: &ArgumentsNode<'pr>) -> Option<(String, (u32, u32))> {
        arguments
            .arguments()
            .iter()
            .filter_map(|argument| argument.as_keyword_hash_node())
            .flat_map(|hash| hash.elements().iter().collect::<Vec<_>>())
            .filter_map(|element| element.as_assoc_node())
            .find_map(|assoc| string_literal(self.source, &assoc.key()))
    }

    /// Which of a resource's seven actions survive `only:` and `except:`.
    fn actions(
        &mut self,
        node: &CallNode<'pr>,
        hosts: &[CallNode<'pr>],
        singular: bool,
    ) -> Vec<&'static str> {
        let default: &[&str] = if singular {
            &["show", "create", "update", "destroy", "new", "edit"]
        } else {
            &[
                "index", "create", "new", "show", "update", "destroy", "edit",
            ]
        };
        let only = self
            .inherited(node, hosts, "only")
            .map(|value| self.list(&value));
        let except = self
            .inherited(node, hosts, "except")
            .map(|value| self.list(&value))
            .unwrap_or_default();
        let kept = only.unwrap_or_else(|| default.iter().map(|it| (*it).to_owned()).collect());
        default
            .iter()
            .copied()
            .filter(|action| {
                kept.iter().any(|it| it == action) && !except.iter().any(|it| it == action)
            })
            .collect()
    }

    /// Walk the concern bodies `wanted` names, in the scope that asked for them.
    ///
    /// The list is taken out of the reader for the duration, which Prism forces — a
    /// `StatementsNode` does not clone, so a body cannot be held while `self` is borrowed to
    /// walk it — and which bounds the recursion for free: a concern that uses `concerns` sees
    /// an empty list, so `concern :a` written in terms of itself walks once instead of forever.
    fn expand(&mut self, wanted: &[String], scope: &Scope, hosts: &mut Vec<CallNode<'pr>>) {
        if wanted.is_empty() || self.concerns.is_empty() {
            return;
        }
        let concerns = std::mem::take(&mut self.concerns);
        for (name, body) in &concerns {
            if wanted.iter().any(|it| it == name) {
                self.walk(Some(body), scope, hosts);
            }
        }
        self.concerns = concerns;
    }

    /// A symbol, a string, or an array of either, as plain words.
    fn list(&self, node: &Node<'pr>) -> Vec<String> {
        if let Some(array) = node.as_array_node() {
            return array
                .elements()
                .iter()
                .filter_map(|element| symbol_or_string(self.source, &element))
                .map(|(word, _)| word)
                .collect();
        }
        symbol_or_string(self.source, node)
            .map(|(word, _)| vec![word])
            .into_iter()
            .flatten()
            .collect()
    }

    /// Every symbol a resource call names, with its own span.
    fn symbols(&self, arguments: &ArgumentsNode<'pr>) -> Vec<(String, (u32, u32))> {
        arguments
            .arguments()
            .iter()
            .take_while(|argument| argument.as_keyword_hash_node().is_none())
            .filter_map(|argument| symbol_or_string(self.source, &argument))
            .collect()
    }

    /// The value a call passes for `name`, or the innermost `with_options` around it that does.
    ///
    /// The same rule the model macros follow, and it is load-bearing here for the
    /// same reason: mastodon writes `with_options only: [:index], concerns: :batch do` around
    /// three resources, and without the merge each of them declares four helpers Rails does not.
    fn inherited(
        &self,
        node: &CallNode<'pr>,
        hosts: &[CallNode<'pr>],
        name: &str,
    ) -> Option<Node<'pr>> {
        keyword(node, name).or_else(|| hosts.iter().rev().find_map(|host| keyword(host, name)))
    }

    /// The same, for a keyword whose value is a word.
    fn inherited_text(
        &self,
        node: &CallNode<'pr>,
        hosts: &[CallNode<'pr>],
        name: &str,
    ) -> Option<(String, (u32, u32))> {
        symbol_or_string(self.source, &self.inherited(node, hosts, name)?)
    }

    /// A call's own line, and how it should be quoted back to a reader.
    ///
    /// The span is the call's start to its **arguments'** end, which is `syntax::header` minus
    /// its two `Option`s: a call this reader reached is receiverless and named, so its own
    /// location starts where its message does, and the caller has already established that it
    /// has arguments. Stopping at the arguments is what keeps a `do … end` out of the target.
    fn spelling(
        &self,
        node: &CallNode<'pr>,
        arguments: &ArgumentsNode<'pr>,
    ) -> (String, (u32, u32)) {
        let at = (
            node.location().start_offset() as u32,
            arguments.location().end_offset() as u32,
        );
        let text = self
            .source
            .get(at.0 as usize..at.1 as usize)
            .unwrap_or_default();
        // One line, however many the call spans: a provenance comment is a sentence.
        let spelled = text
            .split('\n')
            .next()
            .unwrap_or(text)
            .trim_end()
            .to_owned();
        (spelled, at)
    }

    /// Say a helper exists, unless the name is empty or something already claimed it.
    fn declare(&mut self, name: String, at: (u32, u32), name_at: (u32, u32), spelled: &str) {
        if name.is_empty() || !self.named.insert(name.clone()) {
            return;
        }
        self.routes.helpers.push(Helper {
            name,
            at,
            name_at,
            spelled: spelled.to_owned(),
        });
    }
}

/// A call's first argument, when it is a symbol or a plain string.
fn first_symbol_or_string(source: &str, node: &CallNode<'_>) -> Option<(String, (u32, u32))> {
    symbol_or_string(source, &node.arguments()?.arguments().iter().next()?)
}

/// The statements of this call's `do … end`, if it has one.
fn block_body<'pr>(node: &CallNode<'pr>) -> Option<StatementsNode<'pr>> {
    node.block()?.as_block_node()?.body()?.as_statements_node()
}

/// `Mapper.normalize_name`: squeeze the slashes, drop the trailing one, then join with `_`.
///
/// `None` for a path Rails would refuse to turn into an action — anything holding a character
/// that is not a word character, a dash or a slash, which is every dynamic segment (`:id`),
/// every optional group (`(/:page)`) and every format (`.json`).
fn normalize_name(path: &str) -> Option<String> {
    if path.is_empty()
        || !path
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '/')
    {
        return None;
    }
    let mut name = String::with_capacity(path.len());
    for segment in path.split('/').filter(|segment| !segment.is_empty()) {
        if !name.is_empty() {
            name.push('_');
        }
        name.push_str(segment);
    }
    (!name.is_empty()).then(|| name.replace('-', "_"))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    /// Every shape of the DSL this reader understands, in one file.
    ///
    /// The expected list below is **the real router's**: this exact text was drawn by
    /// `ActionDispatch::Routing::RouteSet` and its `named_routes.names` written out, so the
    /// assertion is not a reading of Rails but Rails' own answer. The two names it does not hold
    /// — `pages_one` and `pages_two` — are the one thing that separates them, and they are
    /// interpolated from the block's parameter.
    const FIXTURE: &str = r##"root to: "home#index"
get "about", to: "pages#about"
get "top(/:length)" => "home#top", :as => "top"

concern :flaggable do
  post :flag, on: :member
end

resources :stories, except: [:index], concerns: :flaggable do
  member do
    post :upvote
  end
  collection do
    get "recent", to: "stories#recent"
  end
  resources :comments, only: %i[index show]
  namespace :admin do
    resources :notes, only: [:index]
  end
end

resource :session, only: %i[new create destroy]

namespace :mod do
  get "notes(/:period)", to: "notes#index"
  resources :mails, only: [:index], as: "mod_mails"
end

scope as: "beta" do
  with_options only: [:index] do
    resources :flags
    resources :series
  end
end

if Rails.env.development?
  get "letter_opener", to: "dev#mail"
end

%w[one two].each do |word|
  get "pages/#{word}", to: "pages#show"
  get "static", to: "pages#static"
end

devise_scope :user do
  get "logout", to: "sessions#destroy"
end

resources :option_types do
  resources :option_values, shallow: true
end

mount Sidekiq::Web, at: "/sidekiq"
"##;

    fn named(source: &str) -> Vec<String> {
        let mut names: Vec<String> = read_routes(source, &[], Whose::Own)
            .names()
            .map(str::to_owned)
            .collect();
        names.sort_unstable();
        names
    }

    /// The whole list, not a `contains`, because what has to be legible is where it *stops*.
    #[test]
    fn every_helper_the_routing_dsl_names() {
        assert_eq!(
            named(FIXTURE),
            // Sorted, which puts the reasons out of order; each is named where it lands.
            [
                // A path that is a word, and `root`, which is one by another name.
                "about",
                // `scope as:` prefixes, and `series` is its own plural so its collection takes
                // `_index` — Rails' rule, and why `collection_name` is not just the word.
                "beta_flags",
                "beta_series_index",
                // A shallow nested resource: the member routes lose the nesting and `new` keeps
                // it, because `new` is a level of its own and `member` is what goes shallow.
                "edit_option_type",
                "edit_option_value",
                // `resources :stories, except: [:index]` declares three of the seven.
                "edit_story",
                // A `concern` expanded by `concerns:`, and a `member` block.
                "flag_story",
                // An `if` both arms of which are routes, and an unknown block that is a wrapper.
                "letter_opener",
                "logout",
                // A path Rails cannot name, inside a `namespace`: the scope's own word is the
                // whole candidate, and only the first such route in a scope gets it.
                "mod",
                // `as:` replaces a resource's word rather than decorating it.
                "mod_mod_mails",
                "new_option_type",
                "new_option_type_option_value",
                // A singular `resource` has no index and its member name is the word itself.
                "new_session",
                "new_story",
                "option_type",
                "option_type_option_values",
                "option_types",
                "option_value",
                // A `collection` block.
                "recent_stories",
                "root",
                "session",
                // A literal array with a block, walked once: the interpolated path declines and
                // the literal one is named.
                "static",
                // The collection route `recent` hangs off, from the nested `resources`.
                "stories",
                "story",
                // Two resources nested in one, and a `namespace` inside a resource block —
                // where the parent's member name joins the prefix *before* the namespace's word.
                "story_admin_notes",
                "story_comment",
                "story_comments",
                // `as:` on a verb names the route outright and the path is not read at all.
                "top",
                "upvote_story",
            ]
            .map(str::to_owned)
        );
    }

    /// `only:` and `except:` decide which of the seven exist, and both are read from a
    /// `with_options` around the call as well as from the call.
    #[test]
    fn which_actions_a_resource_declares() {
        assert_eq!(named("resources :as, only: [:index]\n"), ["as"]);
        assert_eq!(named("resources :as, only: :show\n"), ["a"]);
        assert_eq!(
            named("resources :as, except: %i[index create new edit]\n"),
            ["a"]
        );
        assert_eq!(named("resources :as, only: []\n"), Vec::<String>::new());
        assert_eq!(
            named("with_options only: [:index] do\n  resources :as\n  resources :bs\nend\n"),
            ["as", "bs"]
        );
        // The call's own keyword wins over the block's, which is `OptionMerger`'s order.
        assert_eq!(
            named("with_options only: [:index] do\n  resources :as, only: [:new]\nend\n"),
            ["new_a"]
        );
        // A word that is its own plural takes `_index` for its collection, so that the two
        // routes of `resources :series` are not one name.
        assert_eq!(
            named("resources :series\n"),
            ["edit_series", "new_series", "series", "series_index"]
        );
    }

    /// A singular `resource` declares no index and names its member after itself.
    #[test]
    fn a_singular_resource_is_not_a_collection() {
        assert_eq!(
            named("resource :profile\n"),
            ["edit_profile", "new_profile", "profile"]
        );
        // …and a route written directly in its block is a member route, not a nested one.
        assert_eq!(
            named("resource :profile, only: [] do\n  get :avatar\nend\n"),
            ["avatar_profile"]
        );
    }

    /// Everything that changes the prefix, and the order the words come out in.
    #[test]
    fn what_a_name_is_prefixed_by() {
        assert_eq!(
            named("namespace :admin do\n  resources :as, only: [:index]\nend\n"),
            ["admin_as"]
        );
        assert_eq!(
            named("scope as: :beta do\n  resources :as, only: [:index]\nend\n"),
            ["beta_as"]
        );
        // A `scope` with no `as:` changes nothing about the name, whatever else it says.
        assert_eq!(
            named("scope module: :admin, path: \"x\" do\n  resources :as, only: [:index]\nend\n"),
            ["as"]
        );
        // `namespace` nests first and prefixes second; a plain `scope` never nests at all, so
        // the resource inside it nests itself and the parent's word lands after the scope's.
        assert_eq!(
            named(
                "resources :as, only: [] do\n  namespace :x do\n    resources :bs, only: [:index]\n  end\nend\n"
            ),
            ["a_x_bs"]
        );
        assert_eq!(
            named(
                "resources :as, only: [] do\n  scope as: :x do\n    resources :bs, only: [:index]\n  end\nend\n"
            ),
            ["x_a_bs"]
        );
    }

    /// A verb, in each of the four ways one can be written.
    #[test]
    fn what_a_verb_is_called() {
        assert_eq!(named("get \"about\", to: \"p#a\"\n"), ["about"]);
        assert_eq!(named("get \"/a/b\" => \"p#a\"\n"), ["a_b"]);
        assert_eq!(named("get \"a-b\", to: \"p#a\"\n"), ["a_b"]);
        assert_eq!(named("match \"x\", to: \"p#a\", via: :all\n"), ["x"]);
        // A canonical action names nothing of its own at a level that has a resource, and is an
        // ordinary word anywhere else.
        assert_eq!(
            named(
                "resources :as, only: [] do\n  get :show, on: :member\n  get :new, on: :new\nend\n"
            ),
            ["a", "new_a"]
        );
        assert_eq!(named("get \"show\", to: \"p#a\"\n"), ["show"]);
        // A route written directly in a `resources` block with no `on:` is *nested*: the
        // parent's member name joins the prefix and the action follows it, which is the one
        // level whose join puts the action last.
        assert_eq!(
            named(
                "resources :as, only: [] do\n  get \"preview\", to: \"p#a\"\n  resource :cover, only: [:show]\nend\n"
            ),
            ["a_cover", "a_preview"]
        );
        // A path with a dynamic segment, an optional group or a format names nothing — unless
        // an enclosing scope has a word of its own to give it.
        assert_eq!(named("get \"a/:id\", to: \"p#a\"\n"), Vec::<String>::new());
        assert_eq!(
            named("get \"a(/:id)\", to: \"p#a\"\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            named("resources :as, only: [] do\n  get \"x/:id\", on: :member\nend\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            named("namespace :mod do\n  get \"a/:id\", to: \"p#a\"\nend\n"),
            ["mod"]
        );
        // A call with nothing literal in it at all.
        assert_eq!(named("get root_path, to: \"p#a\"\n"), Vec::<String>::new());
        assert_eq!(named("get\n"), Vec::<String>::new());
        // An empty path is not a word, and a name may begin with an underscore.
        assert_eq!(named("get \"\" => \"p#a\"\n"), Vec::<String>::new());
        assert_eq!(named("get \"_x\", to: \"p#a\"\n"), ["_x"]);
        assert_eq!(named("match \"x\" => \"p#a\", via: :all\n"), ["x"]);
    }

    /// The first route to claim a name keeps it — `has_named_route?` — and this is what makes a
    /// file read twice produce the same document both times.
    #[test]
    fn the_first_route_to_claim_a_name_keeps_it() {
        assert_eq!(
            named(
                "namespace :mod do\n  get \"a(/:x)\", to: \"p#a\"\n  get \"b(/:y)\", to: \"p#b\"\nend\n"
            ),
            ["mod"]
        );
        assert_eq!(
            named("get \"a\", to: \"p#a\"\nget \"a\", to: \"p#b\"\n"),
            ["a"]
        );
    }

    /// `root` is named `root` unless it says otherwise, and it can.
    #[test]
    fn what_root_is_called() {
        assert_eq!(named("root to: \"h#i\"\n"), ["root"]);
        assert_eq!(
            named("root to: \"h#i\", as: \"categories_index\"\n"),
            ["categories_index"]
        );
        assert_eq!(
            named("namespace :admin do\n  root to: \"h#i\"\nend\n"),
            ["admin_root"]
        );
        // Inside a `resources` block the level is `:root`, which puts the collection name first.
        assert_eq!(
            named("resources :as, only: [] do\n  root to: \"h#i\"\nend\n"),
            ["as_root"]
        );
        assert_eq!(named("root\n"), Vec::<String>::new());
    }

    /// The calls that name a helper this reader will not name.
    #[test]
    fn what_declines() {
        assert_eq!(
            named("mount Sidekiq::Web, at: \"/s\"\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            named("direct(:home) { \"https://x\" }\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            named("resolve(\"Story\") { |s| [:story, s] }\n"),
            Vec::<String>::new()
        );
        assert_eq!(named("devise_for :users\n"), Vec::<String>::new());
        // A resource whose name is not a literal, and a namespace that is not one either.
        assert_eq!(named("resources SOME_CONSTANT\n"), Vec::<String>::new());
        // A call of the DSL with no arguments at all.
        assert_eq!(named("resources\n"), Vec::<String>::new());
        assert_eq!(named("concerns\n"), Vec::<String>::new());
        // A literal list with no block is not a loop.
        assert_eq!(named("%w[a b].first\n"), Vec::<String>::new());
        assert_eq!(
            named("namespace SOME_CONSTANT do\n  resources :a\nend\n"),
            Vec::<String>::new()
        );
        // A call on a receiver that is not a literal list is not the DSL at all.
        assert_eq!(
            named("Foo.each do\n  get \"a\", to: \"p#a\"\nend\n"),
            Vec::<String>::new()
        );
        // A name that cannot start a Ruby method.
        assert_eq!(named("get \"1\", to: \"p#a\"\n"), Vec::<String>::new());
    }

    /// `concern` and `concerns`, both spellings, and a concern that names itself.
    #[test]
    fn a_concern_is_expanded_where_it_is_used() {
        let source = "\
concern :flaggable do\n  post :flag, on: :member\nend\n\
concern :searchable do\n  get \"search\", to: \"s#i\"\nend\n\
resources :as, only: [], concerns: :flaggable\n\
namespace :x do\n  concerns :searchable\nend\n";
        assert_eq!(named(source), ["flag_a", "x_search"]);
        // A concern nobody declared expands to nothing, and one that uses itself walks once.
        assert_eq!(
            named("resources :as, only: [], concerns: :nope\n"),
            Vec::<String>::new()
        );
        assert_eq!(
            named("concern :a do\n  concerns :a\n  get \"x\", to: \"p#x\"\nend\nconcerns :a\n"),
            ["x"]
        );
        assert_eq!(
            named("concern do\n  get \"x\", to: \"p#x\"\nend\n"),
            Vec::<String>::new()
        );
    }

    /// A `draw` says which file and at which prefix, and reads nothing itself.
    #[test]
    fn only_a_gems_draw_into_the_application_declares_anything() {
        // A gem engine's own routes file, and the whole rule is the receiver — but *only* in a gem.
        // Four of the seven engines that ship a routes file open with
        // `Rails.application.routes.draw` and their helpers really are the host application's;
        // blazer, pghero and mission_control-jobs draw into their own `Engine.routes` and theirs
        // are reached as `blazer.queries_path` after a `mount`.
        let names = |source: &str, whose| {
            let mut found: Vec<String> = read_routes(source, &[], whose)
                .names()
                .map(str::to_owned)
                .collect();
            found.sort_unstable();
            found
        };
        let application =
            "Rails.application.routes.draw do\n  resources :blobs, only: [:show]\nend\n";
        let engine = "Blazer::Engine.routes.draw do\n  resources :queries, only: [:show]\nend\n";

        assert_eq!(names(application, Whose::Gem), vec!["blob".to_owned()]);
        assert!(names(engine, Whose::Gem).is_empty(), "an engine's own set");

        // The regression this rule could most easily cause, and the reason `Whose` is a
        // parameter rather than a check inside the reader: solidus writes exactly the declined
        // spelling in its **own** `core/config/routes.rb`, for its own routes.
        assert_eq!(names(engine, Whose::Own), vec!["query".to_owned()]);

        // A gem file with no wrapper at all declares nothing rather than falling through to the
        // top-level statements — which `Reader::call` would otherwise walk transparently.
        assert!(names("resources :blobs\n", Whose::Gem).is_empty());
        assert_eq!(
            names("resources :blobs\n", Whose::Own),
            vec!["blob", "blobs", "edit_blob", "new_blob"]
        );

        // `::Rails` counts; a receiver this reader cannot spell declines rather than guesses.
        assert_eq!(
            names(
                "::Rails.application.routes.draw do\n  root to: \"x#y\"\nend\n",
                Whose::Gem
            ),
            vec!["root".to_owned()]
        );
        assert!(
            names("Rails.application.routes.draw\n", Whose::Gem).is_empty(),
            "a `draw` with no block is not a wrapper"
        );

        // The shape that made this differential worth running: activestorage and turbo-rails
        // both end `end if ActiveStorage.draw_routes`, so the wrapper is inside a modifier `if`
        // and a flat scan of the program's statements finds nothing at all. Ten of the
        // twenty-one helpers the app-set engines name were lost to it, in **both** directions —
        // `Reader::walk` skips a call with a receiver, so the workspace fall-through missed it
        // too.
        let guarded =
            "Rails.application.routes.draw do\n  root to: \"x#y\"\nend if Turbo.draw_routes\n";
        assert_eq!(names(guarded, Whose::Gem), vec!["root".to_owned()]);
        assert_eq!(names(guarded, Whose::Own), vec!["root".to_owned()]);
        let unless_guarded =
            "Rails.application.routes.draw do\n  root to: \"x#y\"\nend unless Rails.env.test?\n";
        assert_eq!(names(unless_guarded, Whose::Gem), vec!["root".to_owned()]);
        // And the receiver still decides inside one.
        assert!(
            names(
                "Blazer::Engine.routes.draw do\n  root to: \"x#y\"\nend if Blazer.draw\n",
                Whose::Gem
            )
            .is_empty()
        );
    }

    #[test]
    fn a_draw_says_where_it_is() {
        let routes = read_routes(
            "namespace :api do\n  scope as: :v1 do\n    draw :api\n  end\nend\ndraw :admin\n",
            &[],
            Whose::Own,
        );
        let drawn: Vec<(&str, &[String])> = routes
            .draws()
            .iter()
            .map(|draw| (draw.name.as_str(), draw.prefix.as_slice()))
            .collect();
        assert_eq!(
            drawn,
            [
                ("api", ["api".to_owned(), "v1".to_owned()].as_slice()),
                ("admin", [].as_slice()),
            ]
        );
        assert!(
            read_routes("draw SOME_CONSTANT\n", &[], Whose::Own)
                .draws()
                .is_empty()
        );
        // And a file read at a prefix names everything under it.
        let mut names: Vec<String> = read_routes(
            "resources :as, only: [:index]\n",
            &["api".to_owned()],
            Whose::Own,
        )
        .names()
        .map(str::to_owned)
        .collect();
        names.sort_unstable();
        assert_eq!(names, ["api_as"]);
    }

    /// The wrapper this reader opens with, and the two shapes a routes file can take.
    #[test]
    fn the_draw_block_a_routes_file_opens_with() {
        assert_eq!(
            named("Rails.application.routes.draw do\n  get \"a\", to: \"p#a\"\nend\n"),
            ["a"]
        );
        assert_eq!(
            named("Spree::Core::Engine.routes.draw do\n  get \"a\", to: \"p#a\"\nend\n"),
            ["a"]
        );
        // A drawn file has no wrapper at all, and a `draw` with no block is not one.
        assert_eq!(named("get \"a\", to: \"p#a\"\n"), ["a"]);
        assert_eq!(named(""), Vec::<String>::new());
        assert_eq!(named("class Broken\n"), Vec::<String>::new());
    }

    /// Both arms of a conditional are routes, however the conditional is spelled.
    #[test]
    fn both_arms_of_a_conditional_are_routes() {
        assert_eq!(
            named("if x\n  get \"a\", to: \"p#a\"\nelse\n  get \"b\", to: \"p#b\"\nend\n"),
            ["a", "b"]
        );
        // An arm with nothing in it, on either side.
        assert_eq!(named("if x\nelse\n  get \"b\", to: \"p#b\"\nend\n"), ["b"]);
        assert_eq!(named("if x\n  get \"a\", to: \"p#a\"\nelse\nend\n"), ["a"]);
        assert_eq!(
            named(
                "if x\n  get \"a\", to: \"p#a\"\nelsif y\n  get \"b\", to: \"p#b\"\nelse\n  get \"c\", to: \"p#c\"\nend\n"
            ),
            ["a", "b", "c"]
        );
        assert_eq!(
            named("unless x\n  get \"a\", to: \"p#a\"\nelse\n  get \"b\", to: \"p#b\"\nend\n"),
            ["a", "b"]
        );
        assert_eq!(named("get \"a\", to: \"p#a\" if x\n"), ["a"]);
    }

    /// `shallow` written both ways, and the level it does not reach.
    #[test]
    fn a_shallow_resource_drops_the_nesting_from_its_member_routes() {
        let expected = [
            "edit_a", "edit_b", "new_a", "new_a_b", "a", "a_bs", "as", "b",
        ];
        let mut sorted = expected;
        sorted.sort_unstable();
        assert_eq!(
            named("resources :as do\n  resources :bs, shallow: true\nend\n"),
            sorted
        );
        assert_eq!(
            named("resources :as do\n  shallow do\n    resources :bs\n  end\nend\n"),
            sorted
        );
        assert_eq!(
            named("scope shallow: true do\n  resources :as do\n    resources :bs\n  end\nend\n"),
            sorted
        );
        // …and a `namespace` above it still prefixes the shallow member, because
        // `shallow_prefix` is everything the nesting did not contribute.
        assert_eq!(
            named(
                "namespace :admin do\n  resources :as, only: [] do\n    resources :bs, only: [:show], shallow: true\n  end\nend\n"
            ),
            ["admin_b"]
        );
        assert_eq!(
            named("scope shallow: false do\n  resources :as, only: [:show]\nend\n"),
            ["a"]
        );
    }

    /// A singular `resource` goes shallow too, and its member is the only route that can.
    #[test]
    fn a_shallow_singular_resource() {
        assert_eq!(
            named("resources :as, only: [] do\n  resource :b, only: [:show], shallow: true\nend\n"),
            ["b"]
        );
    }

    /// What the RBS says, and where each declaration points.
    #[test]
    fn what_one_helper_declares() {
        let source = "resources :stories, only: [:index]\n";
        let routes = read_routes(source, &[], Whose::Own);
        let emit: BTreeSet<String> = routes.names().map(str::to_owned).collect();
        let rendered = routes
            .signatures("config/routes.rb", &emit)
            .render(&declaring(&[]));
        assert_eq!(
            rendered.rbs,
            "module RouteHelpers\n  \
             # Rails' route helpers. Rails' own module is anonymous; this one is ya-lsp's, and \
             every method in it points at the line of the routing DSL that named it.\n  \
             # From `config/routes.rb`, `resources :stories, only: [:index]`.\n  \
             def stories_path: (*untyped) -> String\n  \
             # From `config/routes.rb`, `resources :stories, only: [:index]`.\n  \
             def stories_url: (*untyped) -> String\nend\n"
        );
        // Both point at the whole call, and select the symbol inside it.
        assert_eq!(rendered.spans.len(), 2);
        for span in &rendered.spans {
            assert_eq!(
                &source[span.declared.0 as usize..span.declared.1 as usize],
                "resources :stories, only: [:index]"
            );
            assert_eq!(
                &source[span.selection.0 as usize..span.selection.1 as usize],
                "stories"
            );
        }
        // A helper no file was assigned declares nothing, which is how two routes files naming
        // one helper stay one declaration.
        assert!(
            routes
                .signatures("config/routes.rb", &BTreeSet::new())
                .is_empty()
        );
    }

    /// A call spanning several lines is quoted back as one, because a comment is a sentence.
    #[test]
    fn a_provenance_comment_is_one_line() {
        let routes = read_routes(
            "resources :stories,\n          only: [:index]\n",
            &[],
            Whose::Own,
        );
        let emit: BTreeSet<String> = routes.names().map(str::to_owned).collect();
        let rbs = routes
            .signatures("config/routes.rb", &emit)
            .render(&declaring(&[]))
            .rbs;
        assert!(rbs.contains("`resources :stories,`."), "{rbs}");
    }

    /// Which classes and modules the helpers go into, and the three ways a name is not one.
    #[test]
    fn what_hosts_the_route_helpers() {
        let none: [String; 0] = [];
        for (name, superclass, module) in [
            ("StoriesController", None, false),
            (
                "Admin::ArticlesController",
                Some("Admin::ApplicationController"),
                false,
            ),
            (
                "ApplicationController",
                Some("ActionController::Base"),
                false,
            ),
            ("Api::BaseController", Some("ActionController::API"), false),
            // The framework's own base classes are an exact match and not the suffix, which is
            // what reaches a class that subclasses one without being named for it.
            ("Health", Some("ActionController::Base"), false),
            ("Api::V1::Legacy_Controller", None, false),
            ("UserMailer", Some("ApplicationMailer"), false),
            ("StoriesHelper", None, true),
        ] {
            assert!(hosts_routes(name, superclass, &none, module), "{name}");
        }
        for (name, superclass, module) in [
            ("Story", Some("ApplicationRecord"), false),
            ("PlainModule", None, true),
            // A job is not a host: Rails installs the helpers on controllers and mailers.
            ("ImportJob", Some("ApplicationJob"), false),
            // And the name rubydex holds for an anonymous class is not a constant at all, which
            // is the clause that keeps `class …<anonymous>` out of the generated RBS.
            (
                "1640350138339398774:1364<anonymous>",
                Some("ActionController::Base"),
                false,
            ),
            ("StoriesController::<anonymous>", None, false),
            ("lower_caseController", None, false),
        ] {
            assert!(!hosts_routes(name, superclass, &none, module), "{name}");
        }
        // A Sidekiq worker is recognised by a mixin and is still not a host.
        assert!(!hosts_routes(
            "Importer",
            None,
            &["Sidekiq::Job".to_owned()],
            false
        ));
    }

    /// The `include`s, which are a document of nothing else.
    #[test]
    fn where_the_helpers_are_included() {
        let hosts = BTreeSet::from([
            Owner::Instance("StoriesController".to_owned()),
            Owner::Module("ApplicationHelper".to_owned()),
        ]);
        assert_eq!(
            mixins(&hosts).render(&declaring(&[])).rbs,
            "class StoriesController\n  include RouteHelpers\nend\n\
             module ApplicationHelper\n  include RouteHelpers\nend\n"
        );
        assert!(mixins(&BTreeSet::new()).is_empty());
    }
}
