//! The two conventions with no macro at all: a mailer's actions and a job's `perform`.
//!
//! Every other generator in this directory starts from a call; this one starts from a `def` and a
//! superclass. What it writes is entirely **class-side**: Rails answers a mailer's every public
//! action on the class itself, ActiveJob installs `perform_later` and `perform_now` beside a
//! `perform`, and Sidekiq installs `perform_async`, `perform_in` and `perform_at`.
//!
//! # Why the superclass is the gate, and `def perform` is not
//!
//! `perform` is an ordinary method name and applications are full of it — service objects define
//! a public `def perform` with no superclass at all, by the hundred. Declaring `self.perform_later`
//! on those would put a class method on classes that raise it. So the convention is recognised by
//! what a class *inherits* or *includes*, and the `def` is only what it then reads. That is what
//! makes the convention exact rather than a guess.
//!
//! # Sidekiq is not a gap; it is the majority of this half
//!
//! `include Sidekiq::Worker` or `Sidekiq::Job` outnumbers ActiveJob in real applications, so
//! declining it would decline most of the feature. Both spellings are read: `Sidekiq::Job` is
//! what 7.0 renamed `Sidekiq::Worker` to, and both are still written.
//!
//! # What is declined, and why each fails to nothing
//!
//! - **A module.** `include Sidekiq::Worker` in a concern installs the class methods on whoever
//!   includes it, which this pass cannot know — the same wall a concern's `scope` hits, and the
//!   same answer.
//! - **A `def` that is not a statement of the class body**, one inside `private`/`protected`, and
//!   `def self.` — none is an action Rails routes to.
//! - **A method name RBS cannot spell.** `def <=>` in a mailer would render RBS that does not
//!   parse, and [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) refuses
//!   a document it cannot parse *whole* — so one odd name would silence every declaration the file
//!   makes. The guard is load-bearing rather than tidy.

use ruby_prism::{ClassNode, DefNode, Node, ParametersNode, StatementsNode};

use super::syntax::{constant_spelling, def_header, keyword_name, symbol_or_string};
use super::{BASES, INHERITS, WORKERS};
use crate::generated::{Declared, Facts, Owner, Source};

/// What a class inherits or includes, and therefore what is installed on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// `< ApplicationMailer`, `< ActionMailer::Base`. Every public action is a class method.
    Mailer,
    /// `< ApplicationJob`, `< ActiveJob::Base`. `perform_later` and `perform_now`.
    Job,
    /// `include Sidekiq::Worker`, `< SomeBaseWorker`. `perform_async`, `perform_in`, `perform_at`.
    Worker,
}

/// The class a mailer action hands back, spelled the way ActionMailer spells it.
///
/// Real framework text rather than a name this crate invented, which is the difference between
/// it and `Comment::Relation`: when actionmailer is in the bundle and indexed, the gem's own
/// class is what a chain reaches and the stub below adds members to it rather than shadowing
/// it — a declaration with no span is never a place, so the gem keeps every place there is.
pub const MESSAGE_DELIVERY: &str = "ActionMailer::MessageDelivery";

/// What `MessageDelivery` answers, and it is deliberately the four names and nothing else.
///
/// `deliver_now` and `deliver_later` are 183 call sites across the six corpora and the `!`
/// forms are mastodon's 16. Each returns `untyped`: `deliver_now` hands back the `Mail::Message`
/// and `deliver_later` the enqueued job, and both are classes in gems that this crate would be
/// naming rather than reading.
const DELIVERIES: [(&str, &str); 4] = [
    ("deliver_now", "()"),
    ("deliver_now!", "()"),
    ("deliver_later", "(*untyped)"),
    ("deliver_later!", "(*untyped)"),
];

/// The class methods each convention installs, and what each of them returns.
///
/// One table read by one loop, so a name and its type cannot drift apart. The parameter column
/// is `None` for "the `def`'s own" — `perform_later` takes exactly what `perform` takes — and
/// `Some` for the two that prepend one of their own.
const INSTALLS: [(Convention, &str, Option<&str>, &str); 6] = [
    (Convention::Job, "perform_later", None, "untyped"),
    (Convention::Job, "perform_now", None, "untyped"),
    // Sidekiq's client answers with the job id it pushed, or `nil` when a client middleware
    // stopped the push. One class, in Ruby's own signatures, and true of all three.
    (Convention::Worker, "perform_async", None, "String?"),
    (
        Convention::Worker,
        "perform_in",
        Some("(*untyped)"),
        "String?",
    ),
    (
        Convention::Worker,
        "perform_at",
        Some("(*untyped)"),
        "String?",
    ),
    // The mailer's row carries no name: its names are the file's.
    (Convention::Mailer, "", None, MESSAGE_DELIVERY),
];

/// Which convention a class body is, or none.
///
/// The one place the decision is made, and both callers reach it: this file's own reader asks
/// it of what Prism read, and [`analysis::synthesize`](crate::analysis) asks it of what the
/// graph recorded, so which documents are worth opening and which classes are worth reading
/// cannot disagree.
///
/// The order is the whole of the rule. A mixin is asked first, because a class that includes
/// `Sidekiq::Job` and inherits something ending `Job` is a worker and not an ActiveJob. Then
/// the two framework bases, which end in neither suffix. Then the suffixes.
#[must_use]
pub fn convention_of(superclass: Option<&str>, mixins: &[String]) -> Option<Convention> {
    if mixins.iter().any(|name| WORKERS.contains(&name.as_str())) {
        return Some(Convention::Worker);
    }
    let superclass = superclass?;
    BASES
        .iter()
        .find(|(name, _)| *name == superclass)
        .or_else(|| {
            INHERITS
                .iter()
                .find(|(suffix, _)| superclass.ends_with(suffix))
        })
        .map(|(_, convention)| *convention)
}

/// Whether a class writing this superclass is a **mailer**, with no mixin to consider.
///
/// [`convention_of`]'s mailer half asked of the one input a projection of the graph always has:
/// `analysis::views` needs it for `app/views/user_mailer/`, where the question is which of the
/// application's classes a *view directory* may name, and a `Sidekiq::Worker` mixin cannot make
/// a class into a mailer. So the mixins are empty rather than unavailable, and both framework
/// spellings — `< ApplicationMailer` and `< ActionMailer::Base` — still answer, because they
/// are `INHERITS` and `BASES` and not a second table.
#[must_use]
pub fn is_mailer(superclass: &str) -> bool {
    convention_of(Some(superclass), &[]) == Some(Convention::Mailer)
}

/// One public `def` a convention turns into a class method.
#[derive(Debug)]
struct Action {
    name: String,
    /// The RBS parameter list the `def`'s own parameters imply, every type `untyped`.
    parameters: String,
    /// The `def` line, and the method's own name inside it.
    at: (u32, u32),
    name_at: (u32, u32),
}

/// One class in the file, and the `def`s its convention reads.
#[derive(Debug)]
struct Entry {
    /// Spelled with its lexical nesting, exactly as rubydex would spell it.
    name: String,
    convention: Convention,
    actions: Vec<Action>,
    /// The class methods the body already writes, in both spellings Ruby has for one.
    ///
    /// A convention installs a method the class does not have; a class that wrote the same one
    /// itself meant something by it, and its own `def` has a signature, a body and the docs
    /// above it. Declaring over the top would add a second place to jump to and nothing else —
    /// which is not hypothetical: a worker that writes its own `perform_async` inside a
    /// `class << self` to debounce the real one is exactly the position this rule protects.
    singletons: Vec<String>,
}

/// One Ruby file, read for the two conventions. Text in, no graph and no I/O.
#[derive(Debug)]
pub struct Entrypoints {
    classes: Vec<Entry>,
}

/// Read every mailer action and job entry point `source` declares.
#[must_use]
pub fn read_entrypoints(source: &str) -> Entrypoints {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        classes: Vec::new(),
    };
    reader.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    Entrypoints {
        classes: reader.classes,
    }
}

impl Entrypoints {
    /// Whether any class here is a mailer, so the caller can pick the one file that writes the
    /// [`MESSAGE_DELIVERY`] stub. Exactly one may, for the reason exactly one file writes a
    /// relation class: it is one type however many files reach it.
    #[must_use]
    pub fn delivers(&self) -> bool {
        self.classes
            .iter()
            .any(|class| class.convention == Convention::Mailer)
    }

    /// The RBS these conventions declare.
    ///
    /// `delivery` is whether this file is the one to write the [`MESSAGE_DELIVERY`] stub. It is
    /// the caller's decision and not this file's, because the answer depends on every other
    /// file and on whether the application declared that class itself.
    #[must_use]
    pub fn signatures(&self, file: &str, delivery: bool) -> Facts {
        let mut facts = Facts::default();
        for class in &self.classes {
            for action in &class.actions {
                class.declare(&mut facts, file, action);
            }
        }
        if delivery {
            message_delivery(&mut facts);
        }
        facts
    }
}

impl Entry {
    /// Every class method this one `def` installs.
    fn declare(&self, facts: &mut Facts, file: &str, action: &Action) {
        let owner = Owner::Singleton(self.name.clone());
        for (convention, installed, parameters, returns) in INSTALLS {
            if convention != self.convention {
                continue;
            }
            // The mailer's row is the one whose name comes from the file rather than from the
            // table, because its class method *is* the action.
            let name = if installed.is_empty() {
                action.name.clone()
            } else {
                installed.to_owned()
            };
            if self.singletons.contains(&name) {
                continue;
            }
            facts.declare(Declared {
                owner: owner.clone(),
                name,
                returns: returns.to_owned(),
                parameters: parameters.unwrap_or(action.parameters.as_str()).to_owned(),
                because: format!(
                    "From `{file}`, `def {}`. {}",
                    action.name,
                    match convention {
                        Convention::Mailer =>
                            "Rails answers a mailer's every public action on the class.",
                        Convention::Job => "ActiveJob installs it beside `perform`.",
                        Convention::Worker => "Sidekiq installs it beside `perform`.",
                    }
                ),
                at: Some((action.at, action.name_at)),
                from: Source::Convention,
                overloads: Vec::new(),
            });
        }
    }
}

/// The class a mailer action returns, written once for the whole workspace.
///
/// **Nothing here is mapped**, for the reason nothing in a relation class is: no line of
/// anybody's code declares `MessageDelivery#deliver_later`. When actionmailer is indexed the
/// gem's own `def deliver_later` is there too and *it* is the place; this adds the members to
/// the same declaration and adds no place at all, so the two can only agree.
fn message_delivery(facts: &mut Facts) {
    let owner = Owner::Instance(MESSAGE_DELIVERY.to_owned());
    facts.note(
        owner.clone(),
        "What a mailer action hands back. ya-lsp writes this class when ActionMailer is not \
         indexed; no file in the workspace declares it."
            .to_owned(),
    );
    for (name, parameters) in DELIVERIES {
        facts.declare(Declared {
            owner: owner.clone(),
            name: name.to_owned(),
            returns: "untyped".to_owned(),
            parameters: parameters.to_owned(),
            because: String::new(),
            at: None,
            from: Source::Convention,
            overloads: Vec::new(),
        });
    }
}

struct Reader<'src> {
    source: &'src str,
    nesting: Vec<String>,
    classes: Vec<Entry>,
}

impl Reader<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree**, for the reason
    /// [`super::models`] recurses this way: a generic visit descends into every method body in
    /// the file and overflows a 2 MiB stack on a large one. It also says what the bounding rule
    /// says — a `class` inside an `if` is not a statement of the body.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            // A `module` is walked through and never read. `include Sidekiq::Worker` in a
            // concern installs the class methods on whoever includes it, which is the wall a
            // concern's `scope` hits and gets the same answer: declare nothing rather than
            // declare it somewhere no call reaches.
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                if let Some(entry) = self.entry(&class) {
                    self.classes.push(entry);
                }
                (class.constant_path(), class.body())
            } else if let Some(module) = statement.as_module_node() {
                (module.constant_path(), module.body())
            } else {
                continue;
            };
            self.nesting.push(constant_spelling(self.source, &path));
            self.walk(inner);
            self.nesting.pop();
        }
    }

    /// One `class` body: what it inherits, what it includes, and the `def`s that follow from it.
    fn entry(&self, node: &ClassNode<'_>) -> Option<Entry> {
        let name = {
            let path = node.constant_path();
            let mut nesting = self.nesting.clone();
            nesting.push(constant_spelling(self.source, &path));
            nesting.join("::")
        };
        let body = node.body().and_then(|body| body.as_statements_node());
        let superclass = node
            .superclass()
            .map(|superclass| constant_spelling(self.source, &superclass));
        let convention = convention_of(superclass.as_deref(), &self.mixins(body.as_ref()))?;
        let actions = self.actions(body.as_ref(), convention);
        (!actions.is_empty()).then_some(Entry {
            name,
            convention,
            actions,
            singletons: self.singletons(body.as_ref()),
        })
    }

    /// Every class method the body writes itself, in both spellings.
    ///
    /// `def self.perform_async` and `class << self; def perform_async; end; end` are one thing
    /// to Ruby and two shapes to Prism, and the corpus writes the second — so reading only the
    /// first would leave the rule true and the one case it exists for uncovered.
    fn singletons(&self, body: Option<&StatementsNode<'_>>) -> Vec<String> {
        let mut written = Vec::new();
        let Some(body) = body else {
            return written;
        };
        for statement in body.body().iter() {
            if let Some(def) = statement.as_def_node()
                && def.receiver().is_some_and(|on| on.as_self_node().is_some())
            {
                written.push(String::from_utf8_lossy(def.name().as_slice()).into_owned());
            }
            if let Some(singleton) = statement.as_singleton_class_node()
                && singleton.expression().as_self_node().is_some()
                && let Some(inner) = singleton.body().and_then(|it| it.as_statements_node())
            {
                written.extend(
                    inner
                        .body()
                        .iter()
                        .filter_map(|node| node.as_def_node())
                        .filter(|def| def.receiver().is_none())
                        .map(|def| String::from_utf8_lossy(def.name().as_slice()).into_owned()),
                );
            }
        }
        written
    }

    /// Every constant the body `include`s, spelled as written.
    ///
    /// `include` and not `extend` or `prepend`: `Sidekiq::Worker` is documented as an include
    /// and `extend`ing it puts its `included` hook nowhere.
    fn mixins(&self, body: Option<&StatementsNode<'_>>) -> Vec<String> {
        let mut mixins = Vec::new();
        let Some(body) = body else {
            return mixins;
        };
        for statement in body.body().iter() {
            let Some(call) = statement.as_call_node() else {
                continue;
            };
            if call.receiver().is_none()
                && call.name().as_slice() == b"include"
                && let Some(argument) = call
                    .arguments()
                    .and_then(|arguments| arguments.arguments().iter().next())
            {
                mixins.push(constant_spelling(self.source, &argument));
            }
        }
        mixins
    }

    /// The public `def`s of a class body that this convention reads.
    ///
    /// Visibility is the file's own: a bare `private` or `protected` closes the public section,
    /// and `private :welcome` names methods already written. `private def welcome` needs
    /// neither — the `def` is an argument rather than a statement, so it was never collected.
    fn actions(&self, body: Option<&StatementsNode<'_>>, convention: Convention) -> Vec<Action> {
        let mut actions: Vec<Action> = Vec::new();
        let mut visible = true;
        let mut hidden: Vec<String> = Vec::new();
        let Some(body) = body else {
            return actions;
        };
        for statement in body.body().iter() {
            if let Some(call) = statement.as_call_node()
                && call.receiver().is_none()
                && matches!(call.name().as_slice(), b"private" | b"protected")
            {
                match call.arguments() {
                    None => visible = false,
                    Some(arguments) => hidden.extend(
                        arguments
                            .arguments()
                            .iter()
                            .filter_map(|argument| symbol_or_string(self.source, &argument))
                            .map(|(name, _)| name),
                    ),
                }
            }
            if let Some(def) = statement.as_def_node()
                && visible
                && def.receiver().is_none()
                && let Some(action) = self.action(&def, convention)
            {
                actions.push(action);
            }
        }
        actions.retain(|action| !hidden.contains(&action.name));
        actions
    }

    /// One `def`, when this convention reads it.
    fn action(&self, node: &DefNode<'_>, convention: Convention) -> Option<Action> {
        let name = String::from_utf8_lossy(node.name().as_slice()).into_owned();
        let wanted = match convention {
            // A job's only entry point is `perform`, whatever else it defines.
            Convention::Job | Convention::Worker => name == "perform",
            // `initialize` is the one public `def` a mailer has that Rails does not route to.
            Convention::Mailer => name != "initialize",
        };
        if !wanted || !spellable(&name) {
            return None;
        }
        let at = node.name_loc();
        Some(Action {
            parameters: parameters_of(self.source, node.parameters().as_ref()),
            name,
            at: def_header(node),
            name_at: (at.start_offset() as u32, at.end_offset() as u32),
        })
    }
}

/// Whether RBS can spell this method name.
///
/// Every operator Ruby lets a `def` name — `<=>`, `[]`, `+` — reaches here, and the whole file's
/// declarations ride on the answer: `Synthesized::record` parses a generated document whole and
/// refuses all of it if any line does not, so one unspellable name would take a mailer's other
/// eleven actions with it. An action Rails routes to is a plain identifier by construction,
/// because it has to be a template's file name too.
fn spellable(name: &str) -> bool {
    let mut characters = name.strip_suffix(['?', '!']).unwrap_or(name).chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && characters.all(|rest| rest.is_ascii_alphanumeric() || rest == '_')
}

/// The RBS parameter list a `def`'s parameters imply, every type `untyped`.
///
/// The shape and not the types, which is all a convention can know — and all it needs to know,
/// because arity is what `types.rs` matches on and a keyword is not part of it. `perform_later`
/// taking exactly what `perform` takes is the whole requirement, and it is what keeps a
/// two-argument call from being rejected against a zero-argument declaration.
fn parameters_of(source: &str, node: Option<&ParametersNode<'_>>) -> String {
    let Some(node) = node else {
        return "()".to_owned();
    };
    let mut spelled: Vec<String> = Vec::new();
    spelled.extend(node.requireds().iter().map(|_| "untyped".to_owned()));
    spelled.extend(node.optionals().iter().map(|_| "?untyped".to_owned()));
    // A `def`'s rest is a `*rest` or nothing: Prism's `ImplicitRestNode` — the `|a,|` of a
    // block — cannot appear in a method's parameters, so asking which kind this is would put an
    // arm here that no Ruby reaches.
    if node.rest().is_some() {
        spelled.push("*untyped".to_owned());
    }
    // Trailing positionals are required exactly as the leading ones are; RBS keeps them in
    // their own list only so it can say where the optional ones went.
    spelled.extend(node.posts().iter().map(|_| "untyped".to_owned()));
    for keyword in node.keywords().iter() {
        let optional = keyword.as_optional_keyword_parameter_node().is_some();
        let name = keyword_name(source, &keyword);
        spelled.push(format!(
            "{}{name}: untyped",
            if optional { "?" } else { "" }
        ));
    }
    if let Some(rest) = node.keyword_rest() {
        // `**nil` is the third kind this can be, and it says the method takes no keywords at
        // all — so it is exactly the one that adds nothing.
        if rest.as_forwarding_parameter_node().is_some() {
            spelled.push("*untyped".to_owned());
            spelled.push("**untyped".to_owned());
        } else if rest.as_keyword_rest_parameter_node().is_some() {
            spelled.push("**untyped".to_owned());
        }
    }
    format!("({})", spelled.join(", "))
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generated::declaring;

    const MAILER: &str = "\
class UserMailer < ApplicationMailer
  def welcome(user)
    @user = user
    mail(to: user.email)
  end

  def digest(user, since: nil, **options)
    mail(to: user.email)
  end

  def initialize
    super
  end

  def self.blast
  end

  private

  def sender
  end
end
";

    const JOB: &str = "\
class DigestJob < ApplicationJob
  def perform(user_id, force = false)
  end

  def helper
  end
end
";

    const WORKER: &str = "\
class BustCacheWorker
  include Sidekiq::Worker

  def perform(key)
  end
end
";

    fn rbs(source: &str, delivery: bool) -> String {
        read_entrypoints(source)
            .signatures("app/mailers/user_mailer.rb", delivery)
            .render(&declaring(&[]))
            .rbs
    }

    /// Pinned whole, for the reason the model's is: every rule shows up in the text, and asserting
    /// them one predicate at a time is how a change to the shape passes ten green tests.
    #[test]
    fn the_rbs_a_mailer_declares() {
        assert_eq!(
            rbs(MAILER, false),
            "\
class UserMailer
  # From `app/mailers/user_mailer.rb`, `def welcome`. Rails answers a mailer's every public \
action on the class.
  def self.welcome: (untyped) -> ActionMailer::MessageDelivery
  # From `app/mailers/user_mailer.rb`, `def digest`. Rails answers a mailer's every public \
action on the class.
  def self.digest: (untyped, ?since: untyped, **untyped) -> ActionMailer::MessageDelivery
end
"
        );
    }

    /// `initialize` is the one public `def` a mailer has that Rails does not route to;
    /// `def self.` is not an action; and a `def` under a bare `private` is not one either.
    #[test]
    fn what_a_mailer_body_declares_nothing_for() {
        let rbs = rbs(MAILER, false);
        for declined in ["initialize", "blast", "sender"] {
            assert!(!rbs.contains(declined), "{declined} was declared:\n{rbs}");
        }
    }

    #[test]
    fn the_rbs_a_job_declares() {
        assert_eq!(
            rbs(JOB, false),
            "\
class DigestJob
  # From `app/mailers/user_mailer.rb`, `def perform`. ActiveJob installs it beside `perform`.
  def self.perform_later: (untyped, ?untyped) -> untyped
  # From `app/mailers/user_mailer.rb`, `def perform`. ActiveJob installs it beside `perform`.
  def self.perform_now: (untyped, ?untyped) -> untyped
end
"
        );
    }

    #[test]
    fn the_rbs_a_sidekiq_worker_declares() {
        assert_eq!(
            rbs(WORKER, false),
            "\
class BustCacheWorker
  # From `app/mailers/user_mailer.rb`, `def perform`. Sidekiq installs it beside `perform`.
  def self.perform_async: (untyped) -> String?
  # From `app/mailers/user_mailer.rb`, `def perform`. Sidekiq installs it beside `perform`.
  def self.perform_in: (*untyped) -> String?
  # From `app/mailers/user_mailer.rb`, `def perform`. Sidekiq installs it beside `perform`.
  def self.perform_at: (*untyped) -> String?
end
"
        );
    }

    /// The stub, written by exactly one file and mapped to none.
    #[test]
    fn the_rbs_the_message_delivery_stub_declares() {
        let declarations = read_entrypoints(MAILER)
            .signatures("app/mailers/user_mailer.rb", true)
            .render(&declaring(&[]));
        // Joined: `ActionMailer` is a name no file in the *application* writes `module` for,
        // so this crate cannot know its kind and the spelling does not change. An explicit
        // wrapper would declare one, which is measured at 234 chatwoot positions on `Api`,
        // `ActiveStorage` and the rest.
        assert!(
            declarations.rbs.ends_with(
                "\
class ActionMailer::MessageDelivery
  # What a mailer action hands back. ya-lsp writes this class when ActionMailer is not \
indexed; no file in the workspace declares it.
  def deliver_now: () -> untyped
  def deliver_now!: () -> untyped
  def deliver_later: (*untyped) -> untyped
  def deliver_later!: (*untyped) -> untyped
end
"
            ),
            "{}",
            declarations.rbs
        );
        // Two mapped actions and six `def`s: the four deliveries are text this crate invented
        // and no line of anybody's code declares them.
        assert_eq!(declarations.methods, 6);
        assert_eq!(declarations.spans.len(), 2);
        assert!(!read_entrypoints(JOB).delivers());
        assert!(read_entrypoints(MAILER).delivers());
    }

    /// The gate, and the corpus that argues for it: 161 of chatwoot's classes define a public
    /// `def perform` and are service objects, so the superclass is what says a job is a job.
    #[test]
    fn a_class_no_convention_recognises_declares_nothing() {
        for source in [
            "class FilterService\n  def perform(scope)\n  end\nend\n",
            "class Cleanup < ApplicationService\n  def perform\n  end\nend\n",
            // A migration Rails generated for a job — chatwoot has three, and a rule keyed on
            // the class's own name rather than its superclass would have declared on all of
            // them.
            "class EnqueueValidateHooksJob < ActiveRecord::Migration[7.1]\n  def perform\n  \
             end\nend\n",
            // A class inside an `if` is not a statement of the body, and neither is a `def`
            // inside a `def`.
            "if x\n  class LateMailer < ApplicationMailer\n    def welcome\n    end\n  \
             end\nend\n",
            "class OuterJob < ApplicationJob\n  def wrapper\n    def perform\n    end\n  \
             end\nend\n",
            // A module is walked through and never read.
            "module Buster\n  include Sidekiq::Worker\n  def perform\n  end\nend\n",
            // A worker that inherits one and defines nothing has nothing to install.
            "class QuietWorker < BustCacheWorker\nend\n",
        ] {
            assert_eq!(rbs(source, false), "", "declared something for:\n{source}");
        }
    }

    /// The three things that make a class one of these, and the order they are asked in.
    #[test]
    fn what_names_a_convention() {
        let sidekiq = ["Sidekiq::Worker".to_owned()];
        let job = ["Sidekiq::Job".to_owned()];
        assert_eq!(convention_of(None, &sidekiq), Some(Convention::Worker));
        assert_eq!(convention_of(None, &job), Some(Convention::Worker));
        // A mixin outranks a suffix: a class that includes `Sidekiq::Job` and inherits
        // something ending `Job` is a worker, and `perform_later` would raise on it.
        assert_eq!(
            convention_of(Some("ApplicationJob"), &job),
            Some(Convention::Worker)
        );
        assert_eq!(
            convention_of(Some("ApplicationMailer"), &[]),
            Some(Convention::Mailer)
        );
        assert_eq!(
            convention_of(Some("Devise::Mailer"), &[]),
            Some(Convention::Mailer)
        );
        assert_eq!(
            convention_of(Some("ActionMailer::Base"), &[]),
            Some(Convention::Mailer)
        );
        assert_eq!(
            convention_of(Some("ActiveJob::Base"), &[]),
            Some(Convention::Job)
        );
        assert_eq!(
            convention_of(Some("ActivityPub::DeliveryWorker"), &[]),
            Some(Convention::Worker)
        );
        assert_eq!(convention_of(None, &[]), None);
        assert_eq!(convention_of(Some("ApplicationRecord"), &[]), None);
        assert_eq!(
            convention_of(Some("ActiveRecord::Migration[7.1]"), &[]),
            None
        );
        // `extend` and `prepend` are not the shape, so a mixin list that holds neither name
        // says nothing.
        assert_eq!(
            convention_of(None, &["ActiveSupport::Concern".to_owned()]),
            None
        );
    }

    /// Every parameter shape Ruby has, rendered as the shape and not as a type.
    ///
    /// Arity is what `types.rs` matches a call against, so a `perform_later` that claims the
    /// wrong one answers nothing rather than answering wrongly — which makes this the half that
    /// has to be exact.
    #[test]
    fn every_parameter_shape_a_def_can_have() {
        let shapes = [
            ("def perform\nend", "()"),
            ("def perform(a, b)\nend", "(untyped, untyped)"),
            ("def perform(a, b = 1)\nend", "(untyped, ?untyped)"),
            ("def perform(*rest)\nend", "(*untyped)"),
            (
                "def perform(a, *rest, z)\nend",
                "(untyped, *untyped, untyped)",
            ),
            ("def perform(to:)\nend", "(to: untyped)"),
            ("def perform(to: nil)\nend", "(?to: untyped)"),
            ("def perform(**options)\nend", "(**untyped)"),
            // `**nil` says the method takes no keywords at all, so it is the one that adds
            // nothing.
            ("def perform(a, **nil)\nend", "(untyped)"),
            ("def perform(...)\nend", "(*untyped, **untyped)"),
            ("def perform(&block)\nend", "()"),
            // A destructured positional is still one positional.
            ("def perform((a, b), c)\nend", "(untyped, untyped)"),
        ];
        for (def, expected) in shapes {
            let source = format!("class T < ApplicationJob\n  {def}\nend\n");
            let rbs = rbs(&source, false);
            assert!(
                rbs.contains(&format!("def self.perform_later: {expected} -> untyped")),
                "{def} rendered:\n{rbs}"
            );
        }
    }

    /// A name RBS cannot spell takes nothing else with it, which is the point of the guard:
    /// `Synthesized::record` refuses a generated document it cannot parse *whole*.
    #[test]
    fn a_method_name_rbs_cannot_spell_is_the_only_one_declined() {
        let rbs = rbs(
            "\
class OddMailer < ApplicationMailer
  def <=>(other)
  end

  def value=(v)
  end

  def ready?
  end

  def send!
  end

  def welcome
  end
end
",
            false,
        );
        assert!(rbs.contains("def self.ready?:"), "{rbs}");
        assert!(rbs.contains("def self.send!:"), "{rbs}");
        assert!(rbs.contains("def self.welcome:"), "{rbs}");
        assert!(!rbs.contains("<=>"), "{rbs}");
        assert!(!rbs.contains("value="), "{rbs}");
    }

    /// A class that wrote the class method itself keeps its own, in both spellings — and the
    /// case is forem's, which debounces the real `perform_async` behind one of its own.
    #[test]
    fn a_class_method_the_body_already_writes_is_not_installed_over() {
        let bare = rbs(
            "\
class DebouncedWorker
  include Sidekiq::Job

  def self.perform_async(id)
  end

  def perform(id)
  end
end
",
            false,
        );
        assert!(!bare.contains("perform_async"), "{bare}");
        assert!(bare.contains("def self.perform_in:"), "{bare}");
        assert!(bare.contains("def self.perform_at:"), "{bare}");

        let opened = rbs(
            "\
class OpenedWorker
  include Sidekiq::Job

  class << self
    def perform_async(id)
    end
  end

  def perform(id)
  end
end
",
            false,
        );
        assert!(!opened.contains("perform_async"), "{opened}");
        assert!(opened.contains("def self.perform_in:"), "{opened}");

        // A `class << other` is not the class's own singleton, and a `def self.perform` is not
        // one of the names a convention installs — so neither takes anything away.
        // Four things a body can hold that this reads past: a `class << other`, which is not
        // the class's own singleton; an empty `class << self`; a call with a receiver, which is
        // not a statement the class is making about itself; a bare `include`, which is what a
        // half-typed line looks like to a parser that is asked on every keystroke; and a
        // `def self.perform`, which is not one of the names a convention installs.
        let unrelated = rbs(
            "\
class OtherWorker
  include Sidekiq::Job
  include
  Rails.logger.info(\"loaded\")

  class << Logger
    def perform_async(id)
    end
  end

  class << self
  end

  def self.perform(id)
  end

  def perform(id)
  end
end
",
            false,
        );
        assert!(unrelated.contains("def self.perform_async:"), "{unrelated}");

        // The mailer half of the same rule: the installed name is the action's own.
        let mailer = rbs(
            "\
class OwnMailer < ApplicationMailer
  def self.welcome(user)
  end

  def welcome(user)
  end

  def digest
  end
end
",
            false,
        );
        assert!(!mailer.contains("self.welcome"), "{mailer}");
        assert!(mailer.contains("def self.digest:"), "{mailer}");
    }

    /// Visibility is the file's own, in both spellings a class body has for it.
    #[test]
    fn what_the_file_says_is_private_is_not_an_action() {
        let rbs = rbs(
            "\
class NoticeMailer < ApplicationMailer
  def welcome
  end

  def internal
  end
  private :internal

  private def helper
  end

  protected

  def guarded
  end
end
",
            false,
        );
        assert!(rbs.contains("def self.welcome:"), "{rbs}");
        for declined in ["internal", "helper", "guarded"] {
            assert!(!rbs.contains(declined), "{declined} was declared:\n{rbs}");
        }
    }

    /// A class body nested in a module, and a class with no body at all.
    #[test]
    fn a_convention_is_spelled_with_its_nesting() {
        let source = "\
module Admin
  class ReportMailer < ApplicationMailer
    def weekly
    end
  end

  class EmptyMailer < ApplicationMailer
  end
end
";
        let rbs = read_entrypoints(source)
            .signatures("app/mailers/report_mailer.rb", false)
            .render(&declaring(&["Admin"]))
            .rbs;
        assert!(
            rbs.starts_with("module Admin\nclass ReportMailer\n"),
            "{rbs}"
        );
        assert!(!rbs.contains("EmptyMailer"), "{rbs}");
    }

    /// The whole `def` line is the target and the name is the selection, in all three spellings
    /// a `def` header has.
    #[test]
    fn a_class_method_points_at_the_def_that_implied_it() {
        for (source, header) in [
            (
                "class M < ApplicationMailer\n  def welcome(user)\n  end\nend\n",
                "def welcome(user)",
            ),
            (
                "class M < ApplicationMailer\n  def welcome user\n  end\nend\n",
                "def welcome user",
            ),
            (
                "class M < ApplicationMailer\n  def welcome\n  end\nend\n",
                "def welcome",
            ),
        ] {
            let declarations = read_entrypoints(source)
                .signatures("app/mailers/m.rb", false)
                .render(&declaring(&[]));
            let [span] = declarations.spans.as_slice() else {
                panic!("expected one span:\n{source}");
            };
            let (start, end) = span.declared;
            assert_eq!(&source[start as usize..end as usize], header);
            let (start, end) = span.selection;
            assert_eq!(&source[start as usize..end as usize], "welcome");
        }
    }
}
