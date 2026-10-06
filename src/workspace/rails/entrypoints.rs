//! The two conventions with no macro at all: a mailer's actions and a job's `perform`.
//!
//! Every other generator here starts from a call; this one starts from a `def` and a superclass.
//! Everything it writes is **class-side**:
//! - Rails answers a mailer's every public action on the class itself.
//! - ActiveJob installs `perform_later` and `perform_now` beside a `perform`.
//! - Sidekiq installs `perform_async`, `perform_in` and `perform_at`.
//!
//! # Why the superclass is the gate, and `def perform` is not
//!
//! `perform` is an ordinary method name. Service objects define a public `def perform` with no
//! superclass, by the hundred, and declaring `self.perform_later` on them would add a class method
//! that raises. So the convention is recognised by what a class *inherits* or *includes*, and the
//! `def` is only what it then reads. That makes it exact, not a guess.
//!
//! # Sidekiq is not a gap; it is the majority of this half
//!
//! `include Sidekiq::Worker` or `Sidekiq::Job` outnumbers ActiveJob in real applications, so
//! declining it would decline most of the feature. Both spellings are read: Sidekiq 7.0 renamed
//! `Sidekiq::Worker` to `Sidekiq::Job`, and both are still written.
//!
//! # What is declined, and why each fails to nothing
//!
//! - **A module.** `include Sidekiq::Worker` in a concern installs the class methods on whoever
//!   includes it, which this pass cannot know. Same wall, and same answer, as a concern's `scope`.
//! - **A `def` that is not a statement of the class body**, one under `private`/`protected`, and
//!   `def self.`: none is an action Rails routes to.
//! - **A method name RBS cannot spell.** `def <=>` in a mailer would render unparseable RBS, and
//!   [`Synthesized::record`](crate::analysis::synthesized::Synthesized::record) refuses a document
//!   it cannot parse *whole*. One odd name would silence every declaration in the file, so the
//!   guard is load-bearing.

use ruby_prism::{ClassNode, DefNode, Node, StatementsNode};

use super::syntax::{
    constant_spelling, def_span, keyword, parameters_of, spellable, string_literal,
    symbol_or_string,
};
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

/// The class a mailer action returns, spelled as ActionMailer spells it.
///
/// Real framework text, not an invented name like `Comment::Relation`. When actionmailer is in the
/// bundle and indexed, a chain reaches the gem's own class, and the stub below adds members to it
/// without shadowing it. A declaration with no span is never a place, so the gem keeps every place
/// there is.
pub const MESSAGE_DELIVERY: &str = "ActionMailer::MessageDelivery";

/// What `MessageDelivery` answers: deliberately these four names and nothing else.
///
/// `deliver_now` and `deliver_later` are the common calls, and the `!` forms are rarer. Each
/// returns `untyped`: `deliver_now` returns the `Mail::Message` and `deliver_later` the enqueued
/// job, both classes in gems this crate would be naming, not reading.
const DELIVERIES: [(&str, &str); 4] = [
    ("deliver_now", "()"),
    ("deliver_now!", "()"),
    ("deliver_later", "(*untyped)"),
    ("deliver_later!", "(*untyped)"),
];

/// The class methods each convention installs, and what each returns.
///
/// One table, one loop, so a name and its type cannot drift apart. The parameter column is `None`
/// for "the `def`'s own" (`perform_later` takes exactly what `perform` takes) and `Some` for the
/// two that prepend a parameter of their own.
const INSTALLS: [(Convention, &str, Option<&str>, &str); 6] = [
    // ActiveJob hands back what `enqueue` did: the job it made, or `false` where a callback or the
    // adapter stopped the enqueue. A block is handed the job and changes neither.
    (Convention::Job, "perform_later", None, "instance | false"),
    (Convention::Job, "perform_now", None, "untyped"),
    // Sidekiq's client returns the job id it pushed, or `nil` when a client middleware stopped the
    // push. One class, in Ruby's own signatures, and true of all three.
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
/// The one place the decision is made, and both callers use it. This file's reader asks it of what
/// Prism read; [`analysis::synthesize`](crate::analysis) asks it of what the graph recorded. So
/// "which documents are worth opening" and "which classes are worth reading" cannot disagree.
///
/// The order is the rule:
/// 1. A mixin, because a class that includes `Sidekiq::Job` and inherits something ending in `Job`
///    is a worker, not an ActiveJob.
/// 2. The two framework bases, which end in neither suffix.
/// 3. The suffixes.
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
/// [`convention_of`]'s mailer half, asked of the one input a projection of the graph always has.
/// `analysis::views` needs it for `app/views/user_mailer/`: which classes may a *view directory*
/// name? A `Sidekiq::Worker` mixin cannot make a class a mailer, so the mixins are empty, not
/// unavailable. Both framework spellings (`< ApplicationMailer`, `< ActionMailer::Base`) still
/// answer, because they come from `INHERITS` and `BASES`, not a second table.
#[must_use]
pub fn is_mailer(superclass: &str) -> bool {
    convention_of(Some(superclass), &[]) == Some(Convention::Mailer)
}

/// Where a mailer's `default template_path:` puts the views of every mailer below it.
///
/// ActionMailer looks a mailer's views up in `headers[:template_path] || mailer_name`, and `default`
/// merges into a class attribute each subclass inherits, so the nearest class that wrote one decides
/// (`analysis::views`). A lambda is run on the mailer (`instance_exec`), with the mailer as its one
/// argument where it takes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplatePath {
    /// A written directory, shared by every mailer below the class that wrote it.
    Fixed(String),
    /// The mailer's own name (`name.underscore`) between two written parts: one application's
    /// `->(mailer) { "mailers/#{mailer.class.name.underscore}" }`. `nil` is both parts empty, which
    /// is where Rails looks without one.
    Named { before: String, after: String },
    /// Anything else: the views moved somewhere only running Ruby knows.
    Unknown,
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
    /// A convention installs a method the class does not have. A class that wrote the same one
    /// itself meant something by it, and its `def` has a signature, a body and docs. Declaring over
    /// it would only add a second place to jump to. Real case: a worker that writes its own
    /// `perform_async` inside `class << self` to debounce the real one.
    singletons: Vec<String>,
}

/// One Ruby file, read for the two conventions. Text in, no graph and no I/O.
#[derive(Debug)]
pub struct Entrypoints {
    classes: Vec<Entry>,
    /// Each mailer here that wrote a `default template_path:`, and the last one it wrote.
    template_paths: Vec<(String, TemplatePath)>,
}

/// Read every mailer action and job entry point `source` declares.
#[must_use]
pub fn read_entrypoints(source: &str) -> Entrypoints {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut reader = Reader {
        source,
        nesting: Vec::new(),
        classes: Vec::new(),
        template_paths: Vec::new(),
    };
    reader.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    Entrypoints {
        classes: reader.classes,
        template_paths: reader.template_paths,
    }
}

impl Entrypoints {
    /// Every mailer here that moved its views with `default template_path:`, and where to.
    ///
    /// Read by `analysis::views`, not declared: which mailer renders `mailers/notify_mailer/x` is a
    /// question about a path, like the rest of the view convention.
    pub fn template_paths(&self) -> impl Iterator<Item = (&str, &TemplatePath)> {
        self.template_paths
            .iter()
            .map(|(name, path)| (name.as_str(), path))
    }

    /// Whether any class here is a mailer, so the caller can pick the one file that writes the
    /// [`MESSAGE_DELIVERY`] stub. Exactly one may, as with a relation class: it is one type however
    /// many files reach it.
    #[must_use]
    pub fn delivers(&self) -> bool {
        self.classes
            .iter()
            .any(|class| class.convention == Convention::Mailer)
    }

    /// The RBS these conventions declare.
    ///
    /// `delivery` says whether this file writes the [`MESSAGE_DELIVERY`] stub. The caller decides,
    /// because the answer depends on every other file and on whether the application declared that
    /// class itself.
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
            // The mailer's row takes its name from the file, not the table, because its class
            // method *is* the action.
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
                private: false,
            });
        }
    }
}

/// Where a `template_path:` value puts the views: a written string, or a lambda or `proc` whose body
/// is one string around the mailer's own name ([`TemplatePath`]).
fn moved_to(source: &str, value: &Node<'_>) -> TemplatePath {
    if value.as_nil_node().is_some() {
        return TemplatePath::Named {
            before: String::new(),
            after: String::new(),
        };
    }
    if let Some((written, _)) = string_literal(source, value) {
        return TemplatePath::Fixed(written);
    }
    // `->(mailer) { … }`, `-> { … }`, `proc { |mailer| … }`, `lambda { … }`.
    let (parameters, body) = if let Some(lambda) = value.as_lambda_node() {
        (lambda.parameters(), lambda.body())
    } else if let Some(call) = value.as_call_node().filter(|call| {
        call.receiver().is_none() && matches!(call.name().as_slice(), b"proc" | b"lambda")
    }) && let Some(block) = call.block().and_then(|block| block.as_block_node())
    {
        (block.parameters(), block.body())
    } else {
        return TemplatePath::Unknown;
    };
    let parameter = parameters
        .and_then(|parameters| parameters.as_block_parameters_node())
        .and_then(|parameters| parameters.parameters())
        .and_then(|parameters| parameters.requireds().iter().next())
        .and_then(|required| required.as_required_parameter_node())
        .map(|required| String::from_utf8_lossy(required.name().as_slice()).into_owned());
    let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
        return TemplatePath::Unknown;
    };
    let statements: Vec<Node<'_>> = statements.body().iter().collect();
    let [only] = statements.as_slice() else {
        return TemplatePath::Unknown;
    };
    if let Some((written, _)) = string_literal(source, only) {
        return TemplatePath::Fixed(written);
    }
    own_name_between(source, only, parameter.as_deref()).unwrap_or(TemplatePath::Unknown)
}

/// The spellings of a mailer's own name a `template_path` lambda writes, with `it` for the lambda's
/// one argument (the mailer) and `self` for the mailer it runs on.
const OWN_NAMES: [&str; 5] = [
    "it.class.name.underscore",
    "self.class.name.underscore",
    "it.class.mailer_name",
    "self.class.mailer_name",
    "mailer_name",
];

/// `"mailers/#{mailer.class.name.underscore}"`: one interpolation of the mailer's own name
/// ([`OWN_NAMES`]) between written parts. `None` for any other string.
fn own_name_between(
    source: &str,
    node: &Node<'_>,
    parameter: Option<&str>,
) -> Option<TemplatePath> {
    let string = node.as_interpolated_string_node()?;
    let mut before = String::new();
    let mut after = String::new();
    let mut named = false;
    for part in string.parts().iter() {
        if let Some((written, _)) = string_literal(source, &part) {
            if named {
                after.push_str(&written);
            } else {
                before.push_str(&written);
            }
            continue;
        }
        let embedded = part.as_embedded_statements_node()?;
        let statements: Vec<Node<'_>> = embedded.statements()?.body().iter().collect();
        let [only] = statements.as_slice() else {
            return None;
        };
        let location = only.location();
        let spelled: String = source
            .get(location.start_offset()..location.end_offset())?
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect();
        let spelled = match parameter {
            Some(parameter) => spelled
                .strip_prefix(parameter)
                .filter(|rest| rest.starts_with('.'))
                .map_or(spelled.clone(), |rest| format!("it{rest}")),
            None => spelled,
        };
        if named || !OWN_NAMES.contains(&spelled.as_str()) {
            return None;
        }
        named = true;
    }
    named.then_some(TemplatePath::Named { before, after })
}

/// The class a mailer action returns, written once for the whole workspace.
///
/// **Nothing here is mapped**, as with a relation class: no line of anybody's code declares
/// `MessageDelivery#deliver_later`. When actionmailer is indexed, the gem's own `def deliver_later`
/// is the place; this adds members to the same declaration and no place at all, so the two can only
/// agree.
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
            private: false,
        });
    }
}

struct Reader<'src> {
    source: &'src str,
    nesting: Vec<String>,
    classes: Vec<Entry>,
    template_paths: Vec<(String, TemplatePath)>,
}

impl Reader<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree**, for the reason [`super::models`] recurses this
    /// way: a generic visit descends into every method body and overflows a 2 MiB stack on a large
    /// file. It also matches the bounding rule: a `class` inside an `if` is not a statement of the
    /// body.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            // A `module` is walked through, never read. `include Sidekiq::Worker` in a concern
            // installs the class methods on whoever includes it: the wall a concern's `scope` hits,
            // with the same answer. Declare nothing rather than declare it where no call reaches.
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                if let Some(entry) = self.entry(&class) {
                    self.classes.push(entry);
                }
                if let Some(moved) = self.template_path(&class) {
                    self.template_paths.push(moved);
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

    /// One `class` body: what it inherits, what it includes, and the `def`s that follow.
    fn entry(&self, node: &ClassNode<'_>) -> Option<Entry> {
        let body = node.body().and_then(|body| body.as_statements_node());
        let convention = self.convention(node)?;
        let actions = self.actions(body.as_ref(), convention);
        (!actions.is_empty()).then_some(Entry {
            name: self.named(node),
            convention,
            actions,
            singletons: self.singletons(body.as_ref()),
        })
    }

    /// The class spelled with its lexical nesting, as rubydex spells it.
    fn named(&self, node: &ClassNode<'_>) -> String {
        let mut nesting = self.nesting.clone();
        nesting.push(constant_spelling(self.source, &node.constant_path()));
        nesting.join("::")
    }

    /// What the class inherits and includes makes it.
    fn convention(&self, node: &ClassNode<'_>) -> Option<Convention> {
        let body = node.body().and_then(|body| body.as_statements_node());
        let superclass = node
            .superclass()
            .map(|superclass| constant_spelling(self.source, &superclass));
        convention_of(superclass.as_deref(), &self.mixins(body.as_ref()))
    }

    /// A mailer body's last `default … template_path: …` statement, read.
    ///
    /// Only a statement of the body, on the class itself (`self.default` too): `default` merges when
    /// the class loads, and one inside a `def` runs when somebody calls it.
    fn template_path(&self, node: &ClassNode<'_>) -> Option<(String, TemplatePath)> {
        if self.convention(node)? != Convention::Mailer {
            return None;
        }
        let body = node.body()?.as_statements_node()?;
        let written = body
            .body()
            .iter()
            .filter_map(|statement| statement.as_call_node())
            .filter(|call| {
                call.receiver().is_none_or(|on| on.as_self_node().is_some())
                    && call.name().as_slice() == b"default"
            })
            .filter_map(|call| keyword(&call, "template_path"))
            .last()?;
        Some((self.named(node), moved_to(self.source, &written)))
    }

    /// Every class method the body writes itself, in both spellings.
    ///
    /// `def self.perform_async` and `class << self; def perform_async; end; end` are one thing to
    /// Ruby and two shapes to Prism. Real code writes the second, so reading only the first would
    /// leave the rule's one real case uncovered.
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
    /// `include`, not `extend` or `prepend`: `Sidekiq::Worker` is documented as an include, and
    /// `extend`ing it puts its `included` hook nowhere.
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
    /// Visibility is the file's own. A bare `private` or `protected` closes the public section, and
    /// `private :welcome` names methods already written. `private def welcome` needs neither: the
    /// `def` is an argument, not a statement, so it was never collected.
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
            at: def_span(node),
            name_at: (at.start_offset() as u32, at.end_offset() as u32),
        })
    }
}

/// Whether Rails calls `method` on an object with these ancestors with arguments no written call
/// shows, so its parameters are not what its callers pass:
///
/// - **A Sidekiq worker's `perform`**: Sidekiq hands it what `perform_async` was, after a round
///   trip through JSON, which turns a `Symbol` into a `String` and a record into whatever its
///   `to_json` wrote. A non-Rails gem, so its calls are not read ([`run_from_the_class`] reads
///   ActiveJob's).
/// - **A channel's public methods**: ActionCable runs the action a client names, with what the client
///   sent.
#[must_use]
pub fn called_by_rails(method: &str, ancestors: &[&str]) -> bool {
    let worker = ancestors.iter().any(|ancestor| WORKERS.contains(ancestor));
    let channel = ancestors.contains(&"ActionCable::Channel::Base");
    (worker && method == "perform") || channel
}

/// The class methods Rails installs beside `method` on an object with these ancestors that run it
/// on a new instance of the class they are called on, handed what they were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FromTheClass {
    /// Each class method's name.
    pub names: Vec<String>,
    /// Whether something may run the method where no call is written.
    pub unwritten: bool,
}

/// What runs `method` from the class, on an object with these ancestors ([`FromTheClass`]):
///
/// - **A job's `perform`** is run by `perform_later(args)` and `perform_now(args)` on the job's
///   class, and by both after `set(wait: …)` ([`passes_to_the_class`]). `perform_now` hands the
///   arguments straight on.
///   `perform_later`'s go through the queue, and ActiveJob's serializers hand back the class they
///   were given for every type they accept (a record is found again by its `GlobalID`) and raise
///   on any other. A scheduler enqueues a job by its class's name with arguments no Ruby call
///   shows, so the answer always says more may come.
/// - **A mailer's action** is run by the same name on the mailer's class, and after `with(params)`:
///   ActionMailer makes a mailer and calls the action with what the class method was handed, after
///   the same queue for `deliver_later`. `initialize` is no action.
///
/// The convention's class methods are what [`Entrypoints::signatures`] declares, so a call
/// reaching one reaches the `def` it was declared beside. Sidekiq's are not read
/// ([`called_by_rails`]).
#[must_use]
pub fn run_from_the_class(method: &str, ancestors: &[&str]) -> Option<FromTheClass> {
    if ancestors.iter().any(|ancestor| WORKERS.contains(ancestor)) {
        return None;
    }
    if method == "perform" && ancestors.contains(&"ActiveJob::Base") {
        return Some(FromTheClass {
            names: vec!["perform_later".to_owned(), "perform_now".to_owned()],
            unwritten: true,
        });
    }
    if method != "initialize" && ancestors.contains(&"ActionMailer::Base") {
        return Some(FromTheClass {
            names: vec![method.to_owned()],
            unwritten: false,
        });
    }
    None
}

/// Whether what the class method `method` hands back, on the class object of a class with these
/// ancestors, takes the class methods [`Entrypoints::signatures`] declared for that class as the
/// class does, with the same arguments to the same instance method:
///
/// - **A mailer's `with(params)`** is a `Parameterized::Mailer`, which answers each of the mailer's
///   actions with a delivery of it, the parameters set on the mailer first.
/// - **A job's `set(wait: …)`** is a `ConfiguredJob`, whose `perform_later` and `perform_now` make
///   the job as the class's do, with the options set.
///
/// Sidekiq's `set` is a non-Rails gem's ([`called_by_rails`]).
#[must_use]
pub fn passes_to_the_class(method: &str, ancestors: &[&str]) -> bool {
    if ancestors.iter().any(|ancestor| WORKERS.contains(ancestor)) {
        return false;
    }
    (method == "with" && ancestors.contains(&"ActionMailer::Base"))
        || (method == "set" && ancestors.contains(&"ActiveJob::Base"))
}

/// The class method whose calls' keywords a literal key read off `reader` holds: ActionMailer keeps
/// what `with(params)` was handed as the mailer's `params` (`@params ||= {}` where it was made
/// without one), through the queue for `deliver_later`, so `params[:user]` in a mailer is each
/// `with(user: …)`'s value, or `nil`.
#[must_use]
pub fn keyed_by_a_class_call(reader: &str) -> Option<&'static str> {
    (reader == "ActionMailer::Parameterized#params()").then_some("with")
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::testing::*;
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

    /// Pinned whole, as the model's is: every rule shows in the text, and asserting one predicate
    /// at a time lets a change of shape pass ten green tests.
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

    /// Not actions: `initialize` (the one public `def` Rails does not route to), `def self.`, and a
    /// `def` under a bare `private`.
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
  def self.perform_later: (untyped, ?untyped) -> (instance | false)
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
        // Joined: no file in the *application* writes `module` for `ActionMailer`, so this crate
        // cannot know its kind and keeps the spelling. An explicit wrapper would declare one, and
        // wrongly declared wrappers on names like `Api` and `ActiveStorage` cost real answers.
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
        // Two mapped actions and six `def`s: the four deliveries are text this crate invented,
        // declared by no line of anybody's code.
        assert_eq!(declarations.methods, 6);
        assert_eq!(declarations.spans.len(), 2);
        assert!(!read_entrypoints(JOB).delivers());
        assert!(read_entrypoints(MAILER).delivers());
    }

    /// The gate. Service objects routinely define a public `def perform`, so the superclass is what
    /// says a job is a job.
    #[test]
    fn a_class_no_convention_recognises_declares_nothing() {
        for source in [
            "class FilterService\n  def perform(scope)\n  end\nend\n",
            "class Cleanup < ApplicationService\n  def perform\n  end\nend\n",
            // A migration Rails generated for a job. A rule keyed on the class's own name instead
            // of its superclass would declare on it.
            "class EnqueueValidateHooksJob < ActiveRecord::Migration[7.1]\n  def perform\n  \
             end\nend\n",
            // A class inside an `if` is not a statement of the body, and neither is a `def` inside
            // a `def`.
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
        // A mixin outranks a suffix: a class that includes `Sidekiq::Job` and inherits something
        // ending in `Job` is a worker, and `perform_later` would raise on it.
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
        // `extend` and `prepend` are not the shape, so a mixin list holding neither name says
        // nothing.
        assert_eq!(
            convention_of(None, &["ActiveSupport::Concern".to_owned()]),
            None
        );
    }

    /// Every parameter shape Ruby has, rendered as the shape, not as a type.
    ///
    /// `types.rs` matches a call against the arity, so a `perform_later` claiming the wrong one
    /// answers nothing. This is the half that must be exact.
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
            // `**nil` says the method takes no keywords at all, so it adds nothing.
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
                rbs.contains(&format!(
                    "def self.perform_later: {expected} -> (instance | false)"
                )),
                "{def} rendered:\n{rbs}"
            );
        }
    }

    /// A name RBS cannot spell takes nothing else with it: the point of the guard, since
    /// `Synthesized::record` refuses a generated document it cannot parse *whole*.
    ///
    /// **A writer is spellable and is routed.** `ActionMailer::Base` answers every public instance
    /// method on the class, `value=` included, and RBS accepts `def self.value=: (untyped value) -> …`.
    /// `<=>` is the shape the guard exists for.
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
        assert!(rbs.contains("def self.value=:"), "{rbs}");
        assert!(!rbs.contains("<=>"), "{rbs}");
    }

    /// A class that wrote the class method itself keeps its own, in both spellings. Real apps
    /// debounce the real `perform_async` behind one of their own.
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

        // Five things a body can hold that this reads past:
        // - a `class << other`, which is not the class's own singleton;
        // - an empty `class << self`;
        // - a call with a receiver, which is not the class stating something about itself;
        // - a bare `include`, which is what a half-typed line looks like to a parser asked on every
        //   keystroke;
        // - a `def self.perform`, which is not a name a convention installs.
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

    /// The whole `def` is the target and the name is the selection, whatever the header looks like:
    /// the pair every hand-written `def` already answers with.
    #[test]
    fn a_class_method_points_at_the_def_that_implied_it() {
        for (source, header) in [
            (
                "class M < ApplicationMailer\n  def welcome(user)\n  end\nend\n",
                "def welcome(user)\n  end",
            ),
            (
                "class M < ApplicationMailer\n  def welcome user\n  end\nend\n",
                "def welcome user\n  end",
            ),
            (
                "class M < ApplicationMailer\n  def welcome\n  end\nend\n",
                "def welcome\n  end",
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

    #[test]
    fn a_mailer_action_is_a_class_method_that_jumps_to_its_def_and_chains() {
        // A mailer in one expression. Three things must happen at once:
        // 1. the action is a *class* method;
        // 2. the jump lands on the `def` that implied it;
        // 3. the chain continues through `MessageDelivery`, which nothing in this workspace
        //    declares, so the stub carries it.
        let source = "UserMailer.welcome(current_user).deliver_later\n";
        let (mut harness, _story, uri) = models_project(source);
        let mailer = harness.write("app/mailers/user_mailer.rb", MAILERS);
        harness.watch(&[&mailer]);

        assert!(
            harness.has("UserMailer::<UserMailer>#welcome()"),
            "the action is not a class method"
        );
        assert!(
            !harness.has("UserMailer::<UserMailer>#sender()"),
            "a private def is not an action"
        );

        let definition = harness.definition_at(&uri, source, "welcome");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(mailer.as_str()),
            "{definition}"
        );
        // `  def welcome(user)` on line 1, revealed whole, with the name selected after `def `.
        assert_eq!(
            (
                &definition[0]["targetRange"]["start"]["line"],
                &definition[0]["targetRange"]["start"]["character"],
                &definition[0]["targetSelectionRange"]["start"]["character"],
            ),
            (
                &serde_json::json!(1),
                &serde_json::json!(2),
                &serde_json::json!(6),
            ),
            "{definition}"
        );

        let card = card(&mut harness, &uri, source, "deliver_later");
        assert!(
            card.contains("MessageDelivery#deliver_later"),
            "the chain did not reach the stub: {card}"
        );
        assert!(!card.contains("Guessed from name alone"), "{card}");
    }

    #[test]
    fn a_hover_on_a_mailer_action_takes_its_def_s_parameters() {
        // The provenance rule again: the card names the file and the `def` because the *generated
        // RBS* carries a comment above the declaration. Nothing in `hover.rs` knows the word
        // "mailer".
        let source = "UserMailer.welcome(current_user)\n";
        let (mut harness, _story, uri) = models_project(source);
        let mailer = harness.write("app/mailers/user_mailer.rb", MAILERS);
        harness.watch(&[&mailer]);

        let card = card(&mut harness, &uri, source, "welcome");
        assert!(
            card.contains("UserMailer.welcome(user) -> ActionMailer::MessageDelivery"),
            "{card}"
        );
    }

    #[test]
    fn a_jobs_perform_installs_both_entry_points_with_its_own_arity() {
        // The job half. Arity must be exact: an answer is partitioned by the *call's* positional
        // argument count, so a `perform_later` claiming `()` would answer nothing for any real
        // call.
        let source = "job = DigestJob.perform_later(1)\n";
        let (mut harness, _story, uri) = models_project(source);
        let job = harness.write(
            "app/jobs/digest_job.rb",
            "class DigestJob < ApplicationJob\n  \
             def perform(user_id, force = false)\n  \
             end\n\n  \
             def helper\n  end\n\
             end\n",
        );
        harness.watch(&[&job]);

        let rbs = harness.generated_rbs("app/jobs/digest_job.rb");
        assert!(
            rbs.contains("def self.perform_later: (untyped, ?untyped) -> (instance | false)\n"),
            "{rbs}"
        );
        assert!(
            rbs.contains("def self.perform_now: (untyped, ?untyped) -> untyped\n"),
            "{rbs}"
        );
        assert!(
            !rbs.contains("helper"),
            "a job's only entry point is `perform`: {rbs}"
        );

        // ActiveJob hands back the job it enqueued, or `false` where the enqueue was stopped.
        assert_eq!(
            drawn_hints(source, &harness.hints_in(&uri)),
            "job: DigestJob | false = DigestJob.perform_later(1)"
        );

        // Both map to the one `def perform`: that is the convention.
        let definition = harness.definition_at(&uri, source, "perform_later");
        assert_eq!(
            definition[0]["targetUri"],
            serde_json::json!(job.as_str()),
            "{definition}"
        );
        assert_eq!(
            definition[0]["targetSelectionRange"]["start"]["line"],
            serde_json::json!(1),
            "{definition}"
        );
    }

    #[test]
    fn a_sidekiq_worker_is_read_and_a_service_object_named_perform_is_not() {
        // Sidekiq is not a footnote: in real apps most job classes are `include Sidekiq::Worker` or
        // `Sidekiq::Job`. The gate is the superclass or the mixin, never the `def`: service objects
        // define a public `def perform` with nothing above them.
        let (mut harness, _story, _uri) = models_project("");
        let worker = harness.write(
            "app/workers/bust_cache_worker.rb",
            "class BustCacheWorker\n  \
             include Sidekiq::Worker\n\n  \
             def perform(key)\n  end\n\
             end\n",
        );
        let service = harness.write(
            "app/services/filter_service.rb",
            "class FilterService\n  def perform(scope)\n  end\nend\n",
        );
        harness.watch(&[&worker, &service]);

        for installed in ["perform_async", "perform_in", "perform_at"] {
            assert!(
                harness.has(&format!("BustCacheWorker::<BustCacheWorker>#{installed}()")),
                "{installed} was not installed"
            );
        }
        assert!(
            !harness.has("BustCacheWorker::<BustCacheWorker>#perform_later()"),
            "Sidekiq has no ActiveJob entry points"
        );
        for declined in ["perform_async", "perform_later", "perform_now"] {
            assert!(
                !harness.has(&format!("FilterService::<FilterService>#{declined}()")),
                "{declined} was declared on a service object"
            );
        }
    }

    #[test]
    fn the_message_delivery_stub_is_written_once_and_is_never_a_place() {
        // The relation class's bargain, on a real name: one type however many mailers reach it, so
        // exactly one file writes it. Nothing in it is mapped, so when actionmailer *is* indexed
        // the gem keeps every place there is.
        let (mut harness, _story, _uri) = models_project("");
        let first = harness.write("app/mailers/user_mailer.rb", MAILERS);
        let second = harness.write(
            "app/mailers/admin_mailer.rb",
            "class AdminMailer < ActionMailer::Base\n  def alert\n  end\nend\n",
        );
        harness.watch(&[&first, &second]);

        assert!(harness.has("UserMailer::<UserMailer>#welcome()"));
        assert!(
            harness.has("AdminMailer::<AdminMailer>#alert()"),
            "`< ActionMailer::Base` is 19 of the corpus' 53 mailers"
        );
        assert!(harness.has("ActionMailer::MessageDelivery#deliver_now()"));

        // URI order, so `admin_mailer.rb` writes it and `user_mailer.rb` does not.
        let admin = harness.generated_rbs("app/mailers/admin_mailer.rb");
        let user = harness.generated_rbs("app/mailers/user_mailer.rb");
        assert!(
            admin.contains("class ActionMailer::MessageDelivery\n"),
            "{admin}"
        );
        assert!(
            !user.contains("class ActionMailer::MessageDelivery"),
            "{user}"
        );

        // Every declaration in the stub is text this crate invented, so none is offered as a place
        // to jump to.
        let source = "AdminMailer.alert.deliver_now\n";
        let uri = harness.write("app/deliver.rb", source);
        harness.watch(&[&uri]);
        let definition = harness.definition_at(&uri, source, "deliver_now");
        assert!(
            definition.as_array().is_none_or(Vec::is_empty),
            "a generated declaration with no span is not a place: {definition}"
        );
    }

    /// Every shape a mailer's `default template_path:` is read in, and the ones that say only that
    /// the views moved. Only a mailer's own statement counts, and the last one it wrote.
    #[test]
    fn where_a_mailer_s_default_template_path_puts_its_views() {
        let source = r##"
class ApplicationMailer < ActionMailer::Base
  default from: "x"
  default(
    from: -> { email_from },
    template_path: ->(mailer) { "mailers/#{mailer.class.name.underscore}" },
  )
end
class Fixed < ApplicationMailer
  default template_path: "shared"
  default template_path: -> { "later" }
end
class Framed < ApplicationMailer
  default template_path: -> { "emails/#{self.class.name.underscore}/html" }
end
class Procs < ApplicationMailer
  default template_path: proc { |m| "x/#{m.class.mailer_name}" }
end
class Own < ApplicationMailer
  default template_path: lambda { "#{mailer_name}" }
end
class Reset < ApplicationMailer
  default template_path: nil
end
class Its < ApplicationMailer
  default template_path: -> { "y/#{it.class.name.underscore}" }
end
class Other < ApplicationMailer
  default template_path: -> { "mailers/#{something_else}" }
end
class Unlike < ApplicationMailer
  default template_path: ->(mailer) { "mailers/#{mailerx.class.name.underscore}" }
end
class Listed < ApplicationMailer
  default template_path: ["a", "b"]
end
class Twice < ApplicationMailer
  default template_path: -> { "#{mailer_name}/#{mailer_name}" }
end
class Variable < ApplicationMailer
  default template_path: -> { "a/#@folder" }
end
class Crowded < ApplicationMailer
  default template_path: -> { "a/#{x; mailer_name}" }
end
class Blank < ApplicationMailer
  default template_path: -> { "a/#{}" }
end
class Joined < ApplicationMailer
  default template_path: -> { "a" "b" }
end
class Statements < ApplicationMailer
  default template_path: proc { log; "y" }
end
class Empty < ApplicationMailer
  default template_path: -> {}
end
class Called < ApplicationMailer
  default template_path: Paths.for(self)
end
class Passed < ApplicationMailer
  default template_path: proc(&PATHS)
end
class Quiet < ApplicationMailer
  default from: "x"

  def welcome
    default template_path: "y"
  end
end
class Service
  default template_path: "z"
end
class Hollow < ApplicationMailer
end
class Selfish < ApplicationMailer
  self.default template_path: "own"
end
class Foreign < ApplicationMailer
  Other.default template_path: "theirs"
end
"##;
        let named = |before: &str, after: &str| TemplatePath::Named {
            before: before.to_owned(),
            after: after.to_owned(),
        };
        let read = read_entrypoints(source);
        let moved: Vec<(&str, &TemplatePath)> = read.template_paths().collect();
        assert_eq!(
            moved,
            [
                ("ApplicationMailer", &named("mailers/", "")),
                ("Fixed", &TemplatePath::Fixed("later".to_owned())),
                ("Framed", &named("emails/", "/html")),
                ("Procs", &named("x/", "")),
                ("Own", &named("", "")),
                ("Reset", &named("", "")),
                ("Its", &named("y/", "")),
                ("Other", &TemplatePath::Unknown),
                ("Unlike", &TemplatePath::Unknown),
                ("Listed", &TemplatePath::Unknown),
                ("Twice", &TemplatePath::Unknown),
                ("Variable", &TemplatePath::Unknown),
                ("Crowded", &TemplatePath::Unknown),
                ("Blank", &TemplatePath::Unknown),
                ("Joined", &TemplatePath::Unknown),
                ("Statements", &TemplatePath::Unknown),
                ("Empty", &TemplatePath::Unknown),
                ("Called", &TemplatePath::Unknown),
                ("Passed", &TemplatePath::Unknown),
                ("Selfish", &TemplatePath::Fixed("own".to_owned())),
            ]
        );
    }

    #[test]
    fn rails_calls_a_workers_perform_and_a_channels_actions_itself() {
        assert!(!called_by_rails(
            "perform",
            &["MyJob", "ApplicationJob", "ActiveJob::Base"]
        ));
        assert!(called_by_rails("perform", &["MyWorker", "Sidekiq::Job"]));
        assert!(!called_by_rails("enqueue", &["MyJob", "ActiveJob::Base"]));
        assert!(called_by_rails(
            "speak",
            &["ChatChannel", "ActionCable::Channel::Base"]
        ));
        assert!(!called_by_rails("perform", &["Service"]));
    }

    /// A job's `perform` is run by its class's `perform_later` and `perform_now`, also after
    /// `set`; a mailer's action by its own name on the class, also after `with`. Sidekiq's are not
    /// read, and neither is anything else.
    #[test]
    fn a_job_and_a_mailer_run_their_methods_from_the_class() {
        let job = run_from_the_class("perform", &["MyJob", "ApplicationJob", "ActiveJob::Base"])
            .expect("a job");
        assert_eq!(job.names, ["perform_later", "perform_now"]);
        assert!(job.unwritten);
        let mailer =
            run_from_the_class("welcome", &["UserMailer", "ActionMailer::Base"]).expect("a mailer");
        assert_eq!(mailer.names, ["welcome"]);
        assert!(!mailer.unwritten);
        assert_eq!(
            run_from_the_class("initialize", &["UserMailer", "ActionMailer::Base"]),
            None
        );
        assert_eq!(
            run_from_the_class("helper", &["MyJob", "ActiveJob::Base"]),
            None
        );
        assert_eq!(
            run_from_the_class("perform", &["MyWorker", "Sidekiq::Job", "ActiveJob::Base"]),
            None
        );
        assert_eq!(run_from_the_class("perform", &["Service"]), None);
        let job = ["MyJob", "ActiveJob::Base"];
        let mailer = ["UserMailer", "ActionMailer::Base"];
        assert!(passes_to_the_class("set", &job));
        assert!(passes_to_the_class("with", &mailer));
        assert!(!passes_to_the_class("with", &job));
        assert!(!passes_to_the_class("set", &mailer));
        assert!(!passes_to_the_class(
            "set",
            &["MyWorker", "Sidekiq::Job", "ActiveJob::Base"]
        ));
        assert_eq!(
            keyed_by_a_class_call("ActionMailer::Parameterized#params()"),
            Some("with")
        );
        assert_eq!(
            keyed_by_a_class_call("ActionController::StrongParameters#params()"),
            None
        );
    }
}
