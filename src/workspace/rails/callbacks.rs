//! What a controller surely runs before an action: the `before_action`s each body
//! writes, less the `skip_before_action`s that may take one back, read from each class's and
//! module's own statements.
//!
//! Rails keeps one callback chain per class, inherited and edited down the ancestry
//! (`AbstractController::Callbacks`). A callback runs before an action where:
//!
//! 1. **A `before_action`, `prepend_before_action` or `append_before_action` names its method** on
//!    the class or an ancestor (a concern's `included do` counts, as the includer's), with no `if:`
//!    and no `unless:`, and with `only:` and `except:` absent or literal lists that let the action
//!    through. A block, a lambda or an object callback is no method this can read.
//! 2. **No skip may apply**: a `skip_before_action` of the name (or of a name only running Ruby
//!    knows) on the class or an ancestor whose `only:`/`except:` does not leave the action out, a
//!    `skip_callback`/`reset_callbacks` of the `:process_action` chain, or any of those this reader
//!    cannot attribute to a body (in a `def`, under a condition): those may apply anywhere.
//!
//! Ruby order is not followed: a callback written again below a skip stays skipped, which only keeps
//! a `nil` that could have gone.
//!
//! **An action that is itself a callback's method** (any `*_action` naming it) may run as one,
//! before the others, so nothing runs surely before it.
//!
//! Also what a callback, a validation or another macro does with a method's name it is passed
//! ([`spelled_use`]), which the types table asks to read a spelled name as a caller or as none.

use std::collections::BTreeSet;

use ruby_prism::{CallNode, Node, Visit};

use super::syntax::{keyword, symbol_or_string};

/// The macros that may take one back.
const SKIPS: [&str; 3] = ["skip_before_action", "skip_callback", "reset_callbacks"];

/// The macros whose method runs around or after an action: named here only so an action that is
/// one of them is not taken for a plain action.
const OTHERS: [&str; 6] = [
    "after_action",
    "prepend_after_action",
    "append_after_action",
    "around_action",
    "prepend_around_action",
    "append_around_action",
];

/// Every name a document may need this reader for: the three that add a callback run before an
/// action, [`SKIPS`] and [`OTHERS`]. [`super::MACROS`] holds them.
pub const NAMES: [&str; 12] = [
    "before_action",
    "prepend_before_action",
    "append_before_action",
    "skip_before_action",
    "skip_callback",
    "reset_callbacks",
    "after_action",
    "prepend_after_action",
    "append_after_action",
    "around_action",
    "prepend_around_action",
    "append_around_action",
];

/// The callbacks one body wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Callbacks {
    /// Each method a `before_action` family call names with no `if:` or `unless:`.
    pub before: Vec<Before>,
    /// Each skip this body wrote.
    pub skips: Vec<Skip>,
    /// Every method any callback macro of this body names, conditions or not.
    pub named: BTreeSet<String>,
}

impl Callbacks {
    /// Whether this body wrote nothing a controller's chain reads.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.before.is_empty() && self.skips.is_empty() && self.named.is_empty()
    }
}

/// One method run before the actions [`Actions`] lets through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Before {
    pub method: String,
    pub actions: Actions,
}

/// One skip: of a callback by name, or `None` for any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub method: Option<String>,
    pub actions: Actions,
}

/// Which actions a callback or a skip applies to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Actions {
    /// `only:` and `except:` as written: each a list, or absent.
    Listed {
        only: Option<BTreeSet<String>>,
        except: BTreeSet<String>,
    },
    /// A condition only running Ruby answers: `if:`, `unless:`, an `only:` that is not a literal.
    Unknown,
}

impl Actions {
    /// Whether it applies to `action`, or `None` where only running Ruby can say.
    #[must_use]
    pub fn includes(&self, action: &str) -> Option<bool> {
        match self {
            Self::Listed { only, except } => Some(
                only.as_ref().is_none_or(|only| only.contains(action)) && !except.contains(action),
            ),
            Self::Unknown => None,
        }
    }
}

/// What one statement of a body adds, where its call is one of [`NAMES`].
pub fn read(source: &str, call: &CallNode<'_>, called: &str, into: &mut Callbacks) {
    let arguments: Vec<Node<'_>> = call
        .arguments()
        .map(|written| written.arguments().iter().collect())
        .unwrap_or_default();
    let positional: Vec<&Node<'_>> = arguments
        .iter()
        .filter(|argument| argument.as_keyword_hash_node().is_none())
        .collect();
    let names: Vec<Option<String>> = positional
        .iter()
        .map(|argument| symbol_or_string(source, argument).map(|(name, _)| name))
        .collect();
    into.named.extend(names.iter().flatten().cloned());
    if SKIPS.contains(&called) {
        into.skips
            .extend(skipped(called, &names, actions(source, call)));
        return;
    }
    if OTHERS.contains(&called) {
        return;
    }
    let actions = if keyword(call, "if").is_some() || keyword(call, "unless").is_some() {
        Actions::Unknown
    } else {
        actions(source, call)
    };
    if actions == Actions::Unknown {
        return;
    }
    into.before
        .extend(names.into_iter().flatten().map(|method| Before {
            method,
            actions: actions.clone(),
        }));
}

/// The skips one skip-family call makes. `skip_callback` and `reset_callbacks` edit a chain their
/// first argument names: only `:process_action` is an action's, and a chain nothing names may be.
fn skipped(called: &str, names: &[Option<String>], actions: Actions) -> Vec<Skip> {
    let names = match called {
        "skip_before_action" => names,
        _ => match names.split_first() {
            Some((Some(chain), _)) if chain != "process_action" => return Vec::new(),
            Some((_, rest)) if called == "skip_callback" => rest.get(1..).unwrap_or_default(),
            _ => &[],
        },
    };
    let every = names.is_empty() || names.iter().any(Option::is_none);
    if every {
        return vec![Skip {
            method: None,
            actions: Actions::Unknown,
        }];
    }
    names
        .iter()
        .flatten()
        .map(|method| Skip {
            method: Some(method.clone()),
            actions: actions.clone(),
        })
        .collect()
}

/// A call's `only:` and `except:`, where each is a symbol, a string, or a literal list of them.
/// Any other keyword spelling (a braced hash, a double splat) may carry either.
fn actions(source: &str, call: &CallNode<'_>) -> Actions {
    let spread = call.arguments().is_some_and(|written| {
        written.arguments().iter().any(|argument| {
            argument.as_hash_node().is_some()
                || argument.as_keyword_hash_node().is_some_and(|hash| {
                    hash.elements()
                        .iter()
                        .any(|element| element.as_assoc_splat_node().is_some())
                })
        })
    });
    if spread {
        return Actions::Unknown;
    }
    let listed = |name: &str| -> Option<Option<BTreeSet<String>>> {
        let Some(value) = keyword(call, name) else {
            return Some(None);
        };
        let written: Vec<Node<'_>> = match value.as_array_node() {
            Some(list) => list.elements().iter().collect(),
            None => vec![value],
        };
        written
            .iter()
            .map(|element| symbol_or_string(source, element).map(|(name, _)| name))
            .collect::<Option<BTreeSet<String>>>()
            .map(Some)
    };
    match (listed("only"), listed("except")) {
        (Some(only), Some(except)) => Actions::Listed {
            only,
            except: except.unwrap_or_default(),
        },
        _ => Actions::Unknown,
    }
}

/// The callback calls of a document no body's statements hold (in a `def`, under a condition, in a
/// `with_options`), whose class or actions this cannot say: each skip may apply to any class and
/// action, and each name may be any class's callback. No `before` is kept. `seen` are the starts
/// of the calls the bodies did read.
#[must_use]
pub fn loose(source: &str, root: &Node<'_>, seen: &BTreeSet<u32>) -> Callbacks {
    struct Calls<'s, 'r> {
        source: &'s str,
        seen: &'r BTreeSet<u32>,
        found: Callbacks,
    }
    impl<'pr> Visit<'pr> for Calls<'_, '_> {
        fn visit_call_node(&mut self, node: &CallNode<'pr>) {
            let called = String::from_utf8_lossy(node.name().as_slice()).into_owned();
            if NAMES.contains(&called.as_str())
                && !self.seen.contains(&(node.location().start_offset() as u32))
            {
                let mut callbacks = Callbacks::default();
                read(self.source, node, &called, &mut callbacks);
                self.found.named.extend(callbacks.named);
                self.found
                    .skips
                    .extend(callbacks.skips.into_iter().map(|skip| Skip {
                        method: skip.method,
                        actions: Actions::Unknown,
                    }));
            }
            ruby_prism::visit_call_node(self, node);
        }
    }
    let mut calls = Calls {
        source,
        seen,
        found: Callbacks::default(),
    };
    calls.visit(root);
    calls.found
}

/// Another document's [`loose`] calls joined into these.
pub fn absorb(into: &mut Callbacks, other: &Callbacks) {
    into.skips.extend(other.skips.iter().cloned());
    into.named.extend(other.named.iter().cloned());
}

/// The methods that surely run before `action` on an object whose ancestry is `chain` (its own
/// class first, every link [`Callbacks`] or `None` for a body that wrote none), given the
/// [`loose`] calls of every document. Empty where the action is itself a callback's method.
#[must_use]
pub fn runs_before(chain: &[Option<&Callbacks>], loose: &Callbacks, action: &str) -> Vec<String> {
    let bodies = || chain.iter().flatten().chain([&loose]);
    if bodies().any(|body| body.named.contains(action)) {
        return Vec::new();
    }
    let skipped = |method: &str| {
        bodies().flat_map(|body| body.skips.iter()).any(|skip| {
            skip.method.as_deref().is_none_or(|name| name == method)
                && skip.actions.includes(action) != Some(false)
        })
    };
    let mut methods: Vec<String> = Vec::new();
    for before in bodies().flat_map(|body| body.before.iter()) {
        if before.actions.includes(action) == Some(true)
            && !skipped(&before.method)
            && !methods.contains(&before.method)
        {
            methods.push(before.method.clone());
        }
    }
    methods
}

/// What Rails does with a method's name one of its macros is passed ([`spelled_use`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameUse {
    /// It reads, filters or defines by the name, and calls nothing by it.
    Named,
    /// It calls the method with no arguments on an object of the class whose body writes the
    /// macro: ActiveSupport's callbacks `send` a symbol with nothing, and so do their `if:` and
    /// `unless:`, and a validation reads each attribute it names.
    CalledBare,
}

/// The macros that add a callback around an action, or a record's or a model's own.
const CALLED_BARE: [&str; 33] = [
    "before_action",
    "prepend_before_action",
    "append_before_action",
    "after_action",
    "prepend_after_action",
    "append_after_action",
    "around_action",
    "prepend_around_action",
    "append_around_action",
    "before_validation",
    "after_validation",
    "before_save",
    "around_save",
    "after_save",
    "before_create",
    "around_create",
    "after_create",
    "before_update",
    "around_update",
    "after_update",
    "before_destroy",
    "around_destroy",
    "after_destroy",
    "after_commit",
    "after_rollback",
    "after_create_commit",
    "after_update_commit",
    "after_destroy_commit",
    "after_save_commit",
    "after_initialize",
    "after_find",
    "after_touch",
    "validate",
];

/// The macros that take a callback back by its name.
const UNCALLED_SKIPS: [&str; 4] = [
    "skip_before_action",
    "skip_after_action",
    "skip_around_action",
    "skip_callback",
];

/// Whether `method` is a callback macro (a controller's, a model's, `validate`) or one that takes a
/// callback back: written for what it registers, never for what it returns.
pub(super) fn names_a_callback(method: &str) -> bool {
    CALLED_BARE.contains(&method) || UNCALLED_SKIPS.contains(&method)
}

/// The macros that define a method by each name they are passed, or name an association, a
/// template or an action: Rails calls nothing by any name written in them.
const NAMING: [&str; 25] = [
    "belongs_to",
    "has_many",
    "has_one",
    "has_and_belongs_to_many",
    "scope",
    "enum",
    "attribute",
    "store",
    "store_accessor",
    "serialize",
    "has_secure_password",
    "has_one_attached",
    "has_many_attached",
    "has_rich_text",
    "accepts_nested_attributes_for",
    "attr_readonly",
    "encrypts",
    "normalizes",
    "delegated_type",
    "define_attribute_methods",
    "render",
    "redirect_to",
    "url_for",
    "protect_from_forgery",
    "skip_forgery_protection",
];

/// What Rails does with a method's name `call` is passed, as a positional (`key` `None`) or under
/// `key`; `routes` says the call is in a file the router draws, whose every name is a route's,
/// an action's or a controller's. `None` where this does not say.
///
/// - **A callback** (an action's, a record's, `validate`) and its `if:` and `unless:` call the
///   method with nothing; `only:`, `except:` and `on:` name actions or contexts.
/// - **A validation** reads each attribute it names with nothing; a `scope:` is read as a column
///   or an association.
/// - **A skip** names a callback; **an association, a scope, an enum, an attribute** define methods
///   by their names; **`render` and `redirect_to`** name templates and actions. Their `if:` and
///   `unless:` are still callbacks; an association's `dependent:` is called on another object,
///   which this does not say.
#[must_use]
pub fn spelled_use(call: &str, key: Option<&str>, routes: bool) -> Option<NameUse> {
    if routes {
        return Some(NameUse::Named);
    }
    let validation = call == "validates" || call == "validates_each" || {
        call.starts_with("validates_") && call.ends_with("_of")
    };
    let condition = matches!(key, Some("if" | "unless"));
    if condition && (CALLED_BARE.contains(&call) || UNCALLED_SKIPS.contains(&call) || validation) {
        return Some(NameUse::CalledBare);
    }
    if CALLED_BARE.contains(&call) {
        return match key {
            None => Some(NameUse::CalledBare),
            Some("only" | "except" | "on" | "prepend") => Some(NameUse::Named),
            Some(_) => None,
        };
    }
    if validation {
        return match key {
            None => Some(NameUse::CalledBare),
            Some("on" | "message" | "in" | "within" | "scope") => Some(NameUse::Named),
            Some(_) => None,
        };
    }
    if UNCALLED_SKIPS.contains(&call) {
        return Some(NameUse::Named);
    }
    if NAMING.contains(&call) {
        return match key {
            Some("dependent") => None,
            Some("if" | "unless") => Some(NameUse::CalledBare),
            _ => Some(NameUse::Named),
        };
    }
    None
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn callbacks_of(source: &str) -> Callbacks {
        let parsed = ruby_prism::parse(source.as_bytes());
        let mut into = Callbacks::default();
        for statement in parsed
            .node()
            .as_program_node()
            .expect("a program")
            .statements()
            .body()
            .iter()
        {
            let call = statement.as_call_node().expect("a call");
            let called = String::from_utf8_lossy(call.name().as_slice()).into_owned();
            read(source, &call, &called, &mut into);
        }
        into
    }

    #[test]
    fn a_before_action_runs_before_the_actions_its_lists_let_through() {
        let body = callbacks_of(
            "\
before_action :set_post, only: %i[show edit]
prepend_before_action \"load\", except: :index
append_before_action :audit
before_action :audit
before_action :check, if: :admin?
before_action :other, unless: -> { true }
before_action :listed, only: ACTIONS
before_action :braced, { only: [:show] }
before_action :spread, **OPTIONS
before_action -> { @x = 1 }
after_action :log
around_action :wrap
",
        );
        let chain = [Some(&body)];
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "show"),
            ["set_post", "load", "audit"]
        );
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "index"),
            ["audit"]
        );
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "update"),
            ["load", "audit"]
        );
        // A method a callback names may run as one, before the others.
        assert!(runs_before(&chain, &Callbacks::default(), "wrap").is_empty());
        assert!(runs_before(&chain, &Callbacks::default(), "check").is_empty());
        assert!(runs_before(&chain, &Callbacks::default(), "log").is_empty());
        assert!(!body.is_empty() && Callbacks::default().is_empty());
        assert!(!callbacks_of("skip_before_action :x\n").is_empty());
        assert!(!callbacks_of("after_action :log\n").is_empty());
    }

    #[test]
    fn a_skip_anywhere_on_the_chain_takes_the_callback_back() {
        let parent = callbacks_of("before_action :set_post\nbefore_action :authorize\n");
        let child = callbacks_of(
            "\
skip_before_action :authorize, only: :index
skip_before_action :set_post, except: [:show]
",
        );
        let chain = [Some(&child), None, Some(&parent)];
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "show"),
            ["set_post", "authorize"]
        );
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "index"),
            Vec::<String>::new()
        );
        assert_eq!(
            runs_before(&chain, &Callbacks::default(), "edit"),
            ["authorize"]
        );

        for skip in [
            "skip_before_action :set_post, if: :x?\n",
            "skip_before_action SKIPPED\n",
            "skip_before_action\n",
            "skip_callback :process_action, :before, :set_post\n",
            "skip_callback CHAIN, :before, :set_post\n",
            "reset_callbacks :process_action\n",
        ] {
            let skipping = callbacks_of(skip);
            let chain = [Some(&skipping), Some(&parent)];
            assert!(
                !runs_before(&chain, &Callbacks::default(), "show")
                    .contains(&"set_post".to_owned()),
                "{skip}"
            );
        }
        // Another chain's skip takes nothing back.
        for skip in [
            "skip_callback :save, :before, :set_post\n",
            "reset_callbacks :save\n",
        ] {
            let skipping = callbacks_of(skip);
            let chain = [Some(&skipping), Some(&parent)];
            assert_eq!(
                runs_before(&chain, &Callbacks::default(), "show"),
                ["set_post", "authorize"],
                "{skip}"
            );
        }
    }

    #[test]
    fn a_callback_under_with_options_is_left_to_the_loose_scan() {
        // Its keywords may limit the call, so no body reads it: the skip counts everywhere and the
        // callback nowhere.
        let model = crate::workspace::rails::read_model(
            "\
class PostsController
  with_options only: :show do
    before_action :set_post
    skip_before_action :authorize
  end
  before_action :load
end
",
        );
        let bodies: Vec<(&str, Vec<&str>)> = model
            .callbacks()
            .map(|(name, said)| {
                (
                    name,
                    said.before
                        .iter()
                        .map(|before| before.method.as_str())
                        .collect(),
                )
            })
            .collect();
        assert_eq!(bodies, [("PostsController", vec!["load"])]);
        let loose = model.loose_callbacks();
        assert_eq!(
            loose
                .skips
                .iter()
                .map(|skip| skip.method.as_deref())
                .collect::<Vec<_>>(),
            [Some("authorize")]
        );
        assert!(loose.named.contains("set_post") && loose.before.is_empty());
    }

    #[test]
    fn a_skip_no_body_holds_counts_everywhere() {
        let source = "\
class PostsController
  skip_before_action :read_here
  skip_before_action :guarded if Rails.env.test?

  def self.open!
    skip_before_action :authorize
  end
end
";
        let parsed = ruby_prism::parse(source.as_bytes());
        let read_here = source.find("skip_before_action :read_here").unwrap() as u32;
        let found = loose(source, &parsed.node(), &BTreeSet::from([read_here]));
        let names: Vec<Option<&str>> = found
            .skips
            .iter()
            .map(|skip| skip.method.as_deref())
            .collect();
        assert_eq!(names, [Some("guarded"), Some("authorize")]);
        assert!(
            found
                .skips
                .iter()
                .all(|skip| skip.actions == Actions::Unknown)
        );
        assert!(found.before.is_empty());

        let parent =
            callbacks_of("before_action :authorize\nbefore_action :load\nbefore_action :show\n");
        let mut joined = Callbacks::default();
        absorb(&mut joined, &found);
        assert_eq!(
            runs_before(&[Some(&parent)], &joined, "index"),
            ["load", "show"]
        );
        // A name only a `def` or a condition passes may be any class's callback.
        let named = loose(
            "def self.guard!\n  before_action :index\nend\n",
            &ruby_prism::parse(b"def self.guard!\n  before_action :index\nend\n").node(),
            &BTreeSet::new(),
        );
        absorb(&mut joined, &named);
        assert!(runs_before(&[Some(&parent)], &joined, "index").is_empty());
    }

    #[test]
    fn a_name_a_macro_is_passed_is_called_with_nothing_named_or_not_said() {
        use NameUse::{CalledBare, Named};
        for (call, key, routes, said) in [
            ("before_action", None, false, Some(CalledBare)),
            ("around_action", Some("if"), false, Some(CalledBare)),
            ("after_save", Some("unless"), false, Some(CalledBare)),
            ("validate", None, false, Some(CalledBare)),
            ("before_action", Some("only"), false, Some(Named)),
            ("after_commit", Some("on"), false, Some(Named)),
            ("before_action", Some("with"), false, None),
            ("validates", None, false, Some(CalledBare)),
            ("validates_presence_of", None, false, Some(CalledBare)),
            ("validates_each", Some("if"), false, Some(CalledBare)),
            ("validates", Some("scope"), false, Some(Named)),
            ("validates", Some("inclusion"), false, None),
            ("skip_before_action", None, false, Some(Named)),
            ("skip_before_action", Some("if"), false, Some(CalledBare)),
            ("has_many", Some("through"), false, Some(Named)),
            ("has_many", Some("dependent"), false, None),
            ("scope", Some("if"), false, Some(CalledBare)),
            ("render", None, false, Some(Named)),
            ("resources", Some("only"), true, Some(Named)),
            ("get", None, true, Some(Named)),
            ("get", None, false, None),
            ("delegate", None, false, None),
        ] {
            assert_eq!(spelled_use(call, key, routes), said, "{call} {key:?}");
        }
    }
}
