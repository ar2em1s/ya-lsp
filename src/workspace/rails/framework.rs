//! The framework's own singletons, and the fixed class each hands back.
//!
//! `Rails.root` is a `Pathname`, `Rails.cache` an `ActiveSupport::Cache::Store`, `Time.zone` an
//! `ActiveSupport::TimeZone`. railties and activesupport ship no `sig/`, so
//! [`Types`](crate::analysis::types::Types) has nothing to read and every chain written on one
//! of them stops at its first hop — `Rails.root.join` falls to the name rung and answers with a
//! list of every `join` in the bundle. rubydex has already found the member; what is missing is
//! only what it returns, and that is a sentence this crate can write down.
//!
//! # Why a table and not a rung
//!
//! The answer can be written *in advance*: there is one `Rails.root` and it is a `Pathname` in
//! every Rails application there has ever been. So it goes through the generator route like
//! every other thing this directory knows — RBS text, into [`Types::harvest`] with Ruby's own
//! signatures and every gem's `sig/`, adding no rung and teaching `types.rs` no Rails word.
//!
//! # Which four, and why the rest are declined
//!
//! **The bar is not "is the return knowable" but "does the class the answer names hold the
//! members the call then asks for".** A receiver typed to a class this crate cannot see the
//! members of *displaces* the name-based guess that used to hold the right word — `types.md`
//! records four times that has happened. So each candidate was scored against the members six
//! corpora really call one hop later, and a row is here only if the class answers them:
//!
//! | chain | chained calls | return | answered |
//! | --- | --- | --- | --- |
//! | `Time.zone` | 2,060 | `ActiveSupport::TimeZone` | 100% |
//! | `Rails.root` | 895 | `Pathname` | 100% |
//! | `Rails.cache` | 362 | `ActiveSupport::Cache::Store` | 99% |
//! | `Rails.application` | 682 | the application's own class | 88% as `Rails::Application` |
//!
//! And four are declined, every one of them for the same mechanism — the class that is *really*
//! returned answers the calls through `method_missing` or `define_method`, which rubydex cannot
//! see, so declaring it would trade a working guess for silence:
//!
//! | chain | chained calls | return | answered |
//! | --- | --- | --- | --- |
//! | `Rails.logger` | 1,395 | `ActiveSupport::BroadcastLogger` | **5%** — `info`, `warn`, `error` and `debug` are `method_missing` |
//! | `Rails.env` | 455 | `ActiveSupport::EnvironmentInquirer` | **4%** — `development?` and its two siblings are `define_method` over a constant list |
//! | `Rails.configuration` | 611 | `Rails::Application::Configuration` | **56%** — `config.action_controller` and everything a project adds are `method_missing` |
//! | `Time.current` | 199 | `ActiveSupport::TimeWithZone` | **79%** — `year`, `month` and the calculations forward to `Time` |
//!
//! `Rails.logger` is the one worth stating twice, because it is the largest population of the
//! eight and the temptation is real: `ActiveSupport::Logger` answers **94%** of those calls and
//! is **not what Rails returns** — since 7.1 `Rails.logger` is a `BroadcastLogger`. A rank that
//! is correct-if-true cannot be bought by naming the wrong class.
//!
//! # What is declared, and what is not
//!
//! Only the return type. railties really writes `def self.root`, activesupport really writes
//! `def zone` in `class << self` — so the declaration rubydex already holds has a place, and the
//! definition written here carries [`Declared::at`] `None` and never becomes a second one. This
//! is [`Source::Interface`] for that reason: no file says what these return, whatever else they
//! say.
//!
//! **Both ends are checked against the graph before anything is written.** A workspace whose
//! bundle is not indexed declares nothing rather than conjuring a `module Rails` with no place
//! and no members — the rule [`super::framework_classes`] already applies to the long tail's
//! three gem classes, read the same way.

use ruby_prism::Node;

use crate::generated::{Declared, Facts, Namespaces, Owner, Source};

use super::syntax::constant_spelling;

/// The class every Rails application's own application class inherits.
///
/// Both a table entry's fallback return and the superclass [`application_class`] looks for, and
/// one constant rather than two because they are the same fact: `Rails.application` is an
/// instance of whichever class the project wrote `< Rails::Application` under, and
/// `Rails::Application` itself where it wrote none.
pub(super) const APPLICATION: &str = "Rails::Application";

/// One framework singleton: `(owner, method, what it returns)`.
///
/// Written out whole rather than grown, for [`Source::rank`](crate::generated::Source)'s reason:
/// a table whose rows arrive one at a time is a table nobody can review. The module doc has the
/// measurement each row is here on and the four that are not.
///
/// **Which keyword opens each owner's body is not in the table**, and must not be: railties
/// writes `module Rails` and activesupport reopens Ruby's `class Time` — in
/// `active_support/core_ext/time/zones.rb`, `def zone` inside `class << self`. The render key is
/// `(is_module, name)`, so guessing wrong declares a second constant RBS refuses to hold beside
/// the first. [`Namespaces::opens`] is what the graph says, and it is asked instead.
const SINGLETONS: [(&str, &str, &str); 4] = [
    ("Rails", "root", "Pathname"),
    ("Rails", "cache", "ActiveSupport::Cache::Store"),
    ("Rails", "application", APPLICATION),
    ("Time", "zone", "ActiveSupport::TimeZone"),
];

/// Every constant a row of [`SINGLETONS`] names, on either side of the arrow.
///
/// Asked of the bundle once per pass, exactly as [`super::framework_classes`] is: a name missing
/// from the answer is a row that declares nothing. Both sides are here and not just the return,
/// because declaring `def self.root` on a `Rails` nothing else declares would *invent* the
/// module — a constant with one member and no place, where before there was an honest miss.
#[must_use]
pub fn singleton_classes() -> Vec<&'static str> {
    let mut names: Vec<&'static str> = SINGLETONS
        .iter()
        .flat_map(|(owner, _, returns)| [*owner, *returns])
        .collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// What the framework's singletons return, for the rows this bundle can back.
///
/// `namespaces` is what anything indexed declares — the application's own walk plus the bundle
/// answering [`singleton_classes`] — and `application` is the class the project's own
/// `config/application.rb` declares, when it declares one. A row whose owner or whose return is
/// not declared anywhere writes nothing at all.
#[must_use]
pub fn read_framework(application: Option<&str>, namespaces: &Namespaces) -> Facts {
    let mut facts = Facts::default();
    for (owner, name, returns) in SINGLETONS {
        // The project's own `Lobsters::Application` in place of the framework's base, and only
        // for the row the base belongs to. It is worth the substitution rather than a nicety:
        // an application's own class is where `config.domain` and the rest of what a project
        // hangs off `Rails.application` are written, and those are 12% of every call the six
        // corpora make on it. It needs no second opinion from the graph — it was read off a
        // `class` line in the application's own file.
        let (returns, declared) = match (returns, application) {
            (APPLICATION, Some(own)) => (own, true),
            _ => (returns, namespaces.declares(returns)),
        };
        if !declared || !namespaces.declares(owner) {
            continue;
        }
        facts.declare(Declared {
            owner: if namespaces.opens(owner) {
                Owner::ModuleSingleton(owner.to_owned())
            } else {
                Owner::Singleton(owner.to_owned())
            },
            name: name.to_owned(),
            returns: returns.to_owned(),
            parameters: "()".to_owned(),
            because: format!(
                "`{owner}.{name}` is a `{returns}`. The framework ships no signature for it, \
                 so ya-lsp writes the return type; the method itself is declared in the bundle."
            ),
            at: None,
            from: Source::Interface,
            overloads: Vec::new(),
        });
    }
    facts
}

/// The class a `config/application.rb` writes `< Rails::Application` under, with its nesting.
///
/// `module Lobsters; class Application < Rails::Application` answers `Lobsters::Application`.
/// `None` for a file that declares none, which is every file but the one, and for the engine
/// that has no such file at all — the caller then keeps [`APPLICATION`].
#[must_use]
pub fn application_class(source: &str) -> Option<String> {
    let parsed = ruby_prism::parse(source.as_bytes());
    let mut walker = Walker {
        source,
        nesting: Vec::new(),
        found: None,
    };
    walker.walk(
        parsed
            .node()
            .as_program_node()
            .map(|program| program.statements().as_node()),
    );
    walker.found
}

/// The `class`/`module` walk [`application_class`] is, and nothing else.
struct Walker<'src> {
    source: &'src str,
    nesting: Vec<String>,
    found: Option<String>,
}

impl Walker<'_> {
    /// One body, and then the class and module bodies written as statements of it.
    ///
    /// **Statements, not a walk of the whole tree**, which is [`super::entrypoints`]' rule for
    /// its reason: a generic visit descends into every method body in the file and overflows a
    /// 2 MiB stack on a large one. A `class` written inside an `if` is not a statement of the
    /// body, and a `config/application.rb` does not write one.
    fn walk(&mut self, body: Option<Node<'_>>) {
        let Some(statements) = body.and_then(|body| body.as_statements_node()) else {
            return;
        };
        for statement in statements.body().iter() {
            let (path, inner) = if let Some(class) = statement.as_class_node() {
                if class
                    .superclass()
                    .map(|superclass| constant_spelling(self.source, &superclass))
                    .as_deref()
                    == Some(APPLICATION)
                {
                    let mut nesting = self.nesting.clone();
                    nesting.push(constant_spelling(self.source, &class.constant_path()));
                    self.found = Some(nesting.join("::"));
                    return;
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
            // After the recursion rather than before it: the first application class in the
            // file is the answer, and a second `module` beside the one that held it must not
            // be walked into on the way out.
            if self.found.is_some() {
                return;
            }
        }
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{APPLICATION, application_class, read_framework, singleton_classes};
    use crate::generated::{Namespaces, declaring, declaring_kinds};

    /// Everything a real bundle declares, spelled the way the graph spells it: railties writes
    /// `module Rails`, and `Time`, `Pathname` and the two activesupport classes are classes.
    fn bundle() -> Namespaces {
        declaring_kinds(
            &[
                "Time",
                "Pathname",
                "ActiveSupport::Cache::Store",
                "ActiveSupport::TimeZone",
                APPLICATION,
            ],
            &["Rails"],
        )
    }

    fn rbs(application: Option<&str>, namespaces: &Namespaces) -> String {
        read_framework(application, namespaces)
            .render(&declaring(&["Rails"]))
            .rbs
    }

    /// The whole of what the table declares, pinned as a document.
    ///
    /// Both bodies, both keywords, and the sentence each member carries — which is the shape
    /// every other test here reads one line out of.
    #[test]
    fn the_rbs_the_framework_table_declares() {
        assert_eq!(
            rbs(None, &bundle()),
            "\
module Rails
  # `Rails.root` is a `Pathname`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.root: () -> Pathname
  # `Rails.cache` is a `ActiveSupport::Cache::Store`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.cache: () -> ActiveSupport::Cache::Store
  # `Rails.application` is a `Rails::Application`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.application: () -> Rails::Application
end
class Time
  # `Time.zone` is a `ActiveSupport::TimeZone`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.zone: () -> ActiveSupport::TimeZone
end
"
        );
    }

    /// The application's own class displaces the framework's base, and only for that row.
    #[test]
    fn the_projects_own_application_class_is_what_rails_application_returns() {
        let rbs = rbs(Some("Lobsters::Application"), &bundle());
        assert!(
            rbs.contains("def self.application: () -> Lobsters::Application"),
            "{rbs}"
        );
        assert!(rbs.contains("def self.root: () -> Pathname"), "{rbs}");
    }

    /// An application class nothing else declares is still written, because it was read off a
    /// `class` line in the application's own file rather than looked up.
    #[test]
    fn the_projects_own_class_needs_no_second_opinion() {
        let rbs = rbs(Some("Lobsters::Application"), &declaring(&["Rails"]));
        assert_eq!(
            rbs,
            "\
module Rails
  # `Rails.application` is a `Lobsters::Application`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.application: () -> Lobsters::Application
end
"
        );
    }

    /// A bundle that is not indexed declares nothing at all, rather than inventing a `Rails`
    /// with one member and no place.
    #[test]
    fn nothing_is_declared_on_an_owner_nothing_else_declares() {
        assert_eq!(rbs(None, &Namespaces::default()), "");
    }

    /// The owner is asked about even when the return is there — the case the test above cannot
    /// reach, because a bundle with neither declines on the return first.
    #[test]
    fn an_owner_nothing_declares_is_declined_though_its_return_is_known() {
        assert_eq!(rbs(None, &declaring_kinds(&["Pathname"], &[])), "");
    }

    /// A row whose return class the bundle does not hold is the only one dropped.
    #[test]
    fn a_row_whose_return_is_missing_declines_on_its_own() {
        let rbs = rbs(None, &declaring_kinds(&["Pathname"], &["Rails"]));
        assert_eq!(
            rbs,
            "\
module Rails
  # `Rails.root` is a `Pathname`. The framework ships no signature for it, so ya-lsp writes the return type; the method itself is declared in the bundle.
  def self.root: () -> Pathname
end
"
        );
    }

    /// Which keyword opens a body is the graph's answer and not the table's: a `Rails` every
    /// file spells `class` is opened with `class`.
    #[test]
    fn the_keyword_that_opens_a_body_is_read_rather_than_assumed() {
        let rbs = rbs(
            None,
            &declaring_kinds(
                &["Rails", "Pathname", "Time", "ActiveSupport::TimeZone"],
                &[],
            ),
        );
        assert!(rbs.starts_with("class Rails\n"), "{rbs}");
        assert!(rbs.contains("class Time\n"), "{rbs}");
    }

    /// Both ends of every row, deduplicated, which is what the bundle is asked about.
    #[test]
    fn every_constant_a_row_names_is_asked_of_the_bundle() {
        assert_eq!(
            singleton_classes(),
            vec![
                "ActiveSupport::Cache::Store",
                "ActiveSupport::TimeZone",
                "Pathname",
                "Rails",
                APPLICATION,
                "Time",
            ]
        );
    }

    /// The application class, with the nesting it is written in.
    #[test]
    fn the_application_class_is_spelled_with_its_nesting() {
        assert_eq!(
            application_class(
                "module Lobsters\n  class Application < Rails::Application\n  end\nend\n"
            ),
            Some("Lobsters::Application".to_owned())
        );
    }

    /// A class written at top level, and one nested two deep.
    #[test]
    fn the_application_class_is_found_at_any_depth() {
        assert_eq!(
            application_class("class Application < Rails::Application\nend\n"),
            Some("Application".to_owned())
        );
        assert_eq!(
            application_class(
                "module A\n  module B\n    class App < Rails::Application\n    end\n  end\nend\n"
            ),
            Some("A::B::App".to_owned())
        );
    }

    /// A class inside another class, which is not a shape Rails generates and is still walked:
    /// the walk descends into every class and module body rather than into the ones a
    /// convention expects.
    #[test]
    fn a_class_nested_in_a_class_is_reached() {
        assert_eq!(
            application_class("class Outer\n  class App < Rails::Application\n  end\nend\n"),
            Some("Outer::App".to_owned())
        );
    }

    /// Everything that is not the shape: no superclass, a different one, a body that holds no
    /// class at all, an empty body, and an empty file.
    #[test]
    fn nothing_but_a_rails_application_subclass_answers() {
        assert_eq!(application_class("class Application\nend\n"), None);
        assert_eq!(application_class("class App < Sinatra::Base\nend\n"), None);
        assert_eq!(application_class("require \"rails\"\n"), None);
        assert_eq!(application_class("module Lobsters\nend\n"), None);
        assert_eq!(application_class(""), None);
    }

    /// The first one wins, and the module beside the one that held it is not walked into.
    #[test]
    fn the_first_application_class_is_the_answer() {
        assert_eq!(
            application_class(
                "module A\n  class App < Rails::Application\n  end\nend\n\
                 module B\n  class App < Rails::Application\n  end\nend\n"
            ),
            Some("A::App".to_owned())
        );
    }
}
