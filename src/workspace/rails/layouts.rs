//! Which layout a controller's or a mailer's views are rendered in: ActionView's own lookup
//! (`ActionView::Layouts#_write_layout_method`), read from the `layout` each class wrote and the
//! layout templates that exist.
//!
//! A layout's path names no class (`layouts/application` spells a `LayoutsController` nobody
//! writes), so the view convention cannot say what its `@ivar`s hold or which `helper_method`s it
//! may call. The classes that render in it can, and Rails decides which those are by one rule per
//! class, walked from the class up:
//!
//! 1. **The nearest `layout` written on the class or an ancestor decides**, because `_layout` is a
//!    `class_attribute`: a string names that layout, `false` names none, and a symbol or a lambda
//!    is a method only running Ruby answers, so it may be any layout.
//! 2. **With nothing written (or `layout nil`), the class's own name is looked up**:
//!    `layouts/<controller_path>` where that template exists, else the parent's answer, which ends
//!    at `ApplicationController`'s `layouts/application`.
//! 3. **`only:` and `except:` make the written layout one of two answers**: an action the condition
//!    leaves out takes rule 2, the generated method's `else` branch.
//!
//! What only running Ruby knows (a symbol's method, a per-action `render layout:`) widens the answer
//! and never narrows it.

use std::collections::BTreeSet;

use ruby_prism::{CallNode, Node};

use super::inflect::underscore;
use super::syntax::string_literal;

/// The directory a layout is looked up in, and the word that keeps a name from being put in it.
const LAYOUTS: &str = "layouts";

/// What one `layout` call says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Said {
    /// `layout "admin"`: that layout, as the name its template is found by (`layouts/admin`).
    Named(String),
    /// `layout false`: no layout.
    Nothing,
    /// `layout nil`: as if none were written, so the lookup by the class's own name applies.
    Implied,
    /// `layout :choose_layout`, a lambda, or any other expression: a method or a value only running
    /// Ruby knows, which may name any layout.
    Dynamic,
}

/// One `layout` call: what it says, and whether `only:` or `except:` limits it to some actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub said: Said,
    pub conditional: bool,
}

/// One class or module on a renderer's ancestor chain, nearest first, and the `layout` its body
/// wrote.
pub struct Link<'a> {
    pub name: &'a str,
    /// Whether it is a class: only a class gets a `_layout` method of its own, so only a class's
    /// name is looked up by rule 2. A module's `layout` (written in `included do`) still counts
    /// under rule 1, as the includer's.
    pub class: bool,
    pub layout: Option<&'a Layout>,
}

/// The layouts one renderer's views may be rendered in.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Layouts {
    /// Each by the name its template is found by (`layouts/application`, `spree/layouts/admin`).
    pub named: BTreeSet<String>,
    /// Whether a method only running Ruby answers, so any layout may be one.
    pub any: bool,
}

impl Layouts {
    /// Whether the layout template `logical` may be one of them.
    #[must_use]
    pub fn includes(&self, logical: &str) -> bool {
        self.any || self.named.contains(logical)
    }
}

/// What a `layout` call says. `None` without an argument, which Rails does not accept.
///
/// Only a plain string is a name: an interpolated one is a value only running Ruby knows, and a
/// symbol names the **method** that answers (`layout :choose_layout`), not a layout.
#[must_use]
pub fn read(source: &str, call: &CallNode<'_>) -> Option<Layout> {
    let arguments: Vec<Node<'_>> = call.arguments()?.arguments().iter().collect();
    let (first, rest) = arguments.split_first()?;
    let said = if let Some((name, _)) = string_literal(source, first) {
        Said::Named(normalized(&name))
    } else if first.as_false_node().is_some() {
        Said::Nothing
    } else if first.as_nil_node().is_some() {
        Said::Implied
    } else {
        Said::Dynamic
    };
    Some(Layout {
        said,
        conditional: rest.iter().any(limits),
    })
}

/// Whether an argument after the name limits the layout to some actions.
///
/// Rails reads `only:` and `except:` alone (`LayoutConditions#_conditional_layout?`), so any other
/// key leaves every action with the layout. An argument this cannot read (a braced hash, a double
/// splat, a variable) counts as a limit, which only adds rule 2's answer to the named one.
fn limits(argument: &Node<'_>) -> bool {
    let Some(hash) = argument.as_keyword_hash_node() else {
        return true;
    };
    hash.elements().iter().any(|element| {
        element.as_assoc_node().is_none_or(|assoc| {
            assoc
                .key()
                .as_symbol_node()
                .is_none_or(|key| matches!(key.unescaped(), b"only" | b"except"))
        })
    })
}

/// The layouts a renderer's views are rendered in, by the three rules in the module docs.
///
/// `chain` is the renderer's linearized ancestors, itself first; `exists` says whether a layout
/// template of that name is there to find.
#[must_use]
pub fn layouts_of(chain: &[Link<'_>], exists: &dyn Fn(&str) -> bool) -> Layouts {
    let mut found = Layouts::default();
    // The class whose `_layout` is being asked: the renderer, then each `super`.
    let mut at = 0;
    while let Some(asked) = chain.get(at) {
        let written = chain[at..].iter().find_map(|link| link.layout);
        let falls_through = match written.map(|layout| (&layout.said, layout.conditional)) {
            Some((Said::Dynamic, _)) => {
                found.any = true;
                return found;
            }
            Some((Said::Named(name), conditional)) => {
                found.named.insert(name.clone());
                conditional
            }
            Some((Said::Nothing, conditional)) => conditional,
            Some((Said::Implied, _)) | None => true,
        };
        if !falls_through {
            break;
        }
        if let Some(own) = implied(asked.name).filter(|own| exists(own)) {
            found.named.insert(own);
            break;
        }
        let Some(parent) = (at + 1..chain.len()).find(|next| chain[*next].class) else {
            break;
        };
        at = parent;
    }
    found
}

/// Whether a template, by the name it is found by, can be a layout: its directory is one a layout
/// name is looked up in.
///
/// `layouts/application` and `spree/layouts/admin` are; `stories/show` and `application` are not.
#[must_use]
pub fn is_layout(logical: &str) -> bool {
    logical
        .rsplit_once('/')
        .is_some_and(|(directory, _)| names_layouts(directory))
}

/// The layout a class's own name looks up (`_implied_layout_name`, its `controller_path`):
/// `Admin::UsersController` is `layouts/admin/users`, and `UserMailer` is `layouts/user_mailer`.
///
/// `None` for a name no segment of which can be underscored.
fn implied(class: &str) -> Option<String> {
    let path = class
        .strip_suffix("Controller")
        .unwrap_or(class)
        .split("::")
        .map(underscore)
        .collect::<Option<Vec<String>>>()?
        .join("/");
    Some(normalized(&path))
}

/// The name a layout's template is found by: Rails' `_normalize_layout`, which puts a name in
/// `layouts/` unless it already says `layouts` at a word boundary (`spree/layouts/admin`).
fn normalized(name: &str) -> String {
    if names_layouts(name) {
        name.to_owned()
    } else {
        format!("{LAYOUTS}/{name}")
    }
}

/// Rails' `/\blayouts/`: the word, starting where no word character comes before it.
fn names_layouts(name: &str) -> bool {
    name.match_indices(LAYOUTS).any(|(at, _)| {
        !name[..at]
            .bytes()
            .next_back()
            .is_some_and(|before| before.is_ascii_alphanumeric() || before == b'_')
    })
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    fn layout(source: &str) -> Option<Layout> {
        let parsed = ruby_prism::parse(source.as_bytes());
        let statement = parsed
            .node()
            .as_program_node()
            .and_then(|program| program.statements().body().iter().next())
            .expect("one statement");
        read(source, &statement.as_call_node().expect("a call"))
    }

    fn said(said: Said, conditional: bool) -> Option<Layout> {
        Some(Layout { said, conditional })
    }

    fn named(name: &str) -> Said {
        Said::Named(name.to_owned())
    }

    #[test]
    fn a_layout_call_says_a_name_none_the_lookup_or_whatever_runs() {
        assert_eq!(
            layout("layout \"admin\""),
            said(named("layouts/admin"), false)
        );
        assert_eq!(
            layout("layout 'mailer/base'"),
            said(named("layouts/mailer/base"), false)
        );
        // A name that already says `layouts` is found where it says.
        assert_eq!(
            layout("layout \"spree/layouts/admin\""),
            said(named("spree/layouts/admin"), false)
        );
        // `\b`: a word ending in `layouts` is not the directory.
        assert_eq!(
            layout("layout \"my_layouts\""),
            said(named("layouts/my_layouts"), false)
        );
        assert_eq!(layout("layout false"), said(Said::Nothing, false));
        assert_eq!(layout("layout nil"), said(Said::Implied, false));
        // A symbol is the method that answers, not a layout.
        assert_eq!(layout("layout :choose_layout"), said(Said::Dynamic, false));
        assert_eq!(
            layout("layout -> { admin? ? 'admin' : 'application' }"),
            said(Said::Dynamic, false)
        );
        assert_eq!(layout("layout \"#{theme}\""), said(Said::Dynamic, false));
        assert_eq!(layout("layout"), None);
    }

    #[test]
    fn only_and_except_limit_a_layout_and_what_cannot_be_read_counts_as_a_limit() {
        assert_eq!(
            layout("layout \"admin\", only: :show"),
            said(named("layouts/admin"), true)
        );
        assert_eq!(
            layout("layout false, except: [:index]"),
            said(Said::Nothing, true)
        );
        // Any other key leaves every action with it.
        assert_eq!(
            layout("layout \"admin\", foo: 1"),
            said(named("layouts/admin"), false)
        );
        assert_eq!(
            layout("layout \"admin\", \"only\" => :show"),
            said(named("layouts/admin"), true)
        );
        assert_eq!(
            layout("layout \"admin\", **conditions"),
            said(named("layouts/admin"), true)
        );
        assert_eq!(
            layout("layout \"admin\", { only: :show }"),
            said(named("layouts/admin"), true)
        );
    }

    fn link<'a>(name: &'a str, class: bool, layout: Option<&'a Layout>) -> Link<'a> {
        Link {
            name,
            class,
            layout,
        }
    }

    fn names(layouts: &Layouts) -> Vec<&str> {
        layouts.named.iter().map(String::as_str).collect()
    }

    #[test]
    fn with_nothing_written_each_class_s_own_name_is_looked_up_up_the_chain() {
        let exists =
            |logical: &str| ["layouts/application", "layouts/admin/users"].contains(&logical);
        let stories = [
            link("StoriesController", true, None),
            link("StoriesHelper", false, None),
            link("ApplicationController", true, None),
            link("ActionController::Base", true, None),
        ];
        let found = layouts_of(&stories, &exists);
        assert_eq!(names(&found), ["layouts/application"]);
        assert!(!found.any);
        assert!(found.includes("layouts/application"));
        assert!(!found.includes("layouts/admin"));

        // The class's own layout wins over its parent's.
        let users = [
            link("Admin::UsersController", true, None),
            link("ApplicationController", true, None),
        ];
        assert_eq!(names(&layouts_of(&users, &exists)), ["layouts/admin/users"]);

        // A mailer whose chain finds no template has no layout.
        let mailer = [
            link("UserMailer", true, None),
            link("ActionMailer::Base", true, None),
        ];
        assert_eq!(layouts_of(&mailer, &exists), Layouts::default());
        // Nor does a chain that is not there, or a name that is not a constant's.
        assert_eq!(layouts_of(&[], &exists), Layouts::default());
        assert_eq!(
            layouts_of(&[link("lower", true, None)], &exists),
            Layouts::default()
        );
    }

    #[test]
    fn the_nearest_written_layout_decides_and_a_limited_one_adds_the_lookup() {
        let exists = |logical: &str| ["layouts/application", "layouts/feeds"].contains(&logical);
        let admin = Layout {
            said: named("layouts/admin"),
            conditional: false,
        };
        let limited = Layout {
            said: named("layouts/admin"),
            conditional: true,
        };
        let none = Layout {
            said: Said::Nothing,
            conditional: false,
        };
        let none_limited = Layout {
            said: Said::Nothing,
            conditional: true,
        };
        let reset = Layout {
            said: Said::Implied,
            conditional: false,
        };
        let dynamic = Layout {
            said: Said::Dynamic,
            conditional: true,
        };

        // Written on the parent, so the child's own name is never looked up.
        let inherited = [
            link("FeedsController", true, None),
            link("Admin::BaseController", true, Some(&admin)),
            link("ApplicationController", true, None),
        ];
        assert_eq!(names(&layouts_of(&inherited, &exists)), ["layouts/admin"]);

        // A concern's `included do` writes it for the includer.
        let included = [
            link("StoriesController", true, None),
            link("Themed", false, Some(&admin)),
            link("ApplicationController", true, None),
        ];
        assert_eq!(names(&layouts_of(&included, &exists)), ["layouts/admin"]);

        // `only:` leaves the other actions to the lookup, from the asking class up.
        let partly = [
            link("FeedsController", true, Some(&limited)),
            link("ApplicationController", true, None),
        ];
        assert_eq!(
            names(&layouts_of(&partly, &exists)),
            ["layouts/admin", "layouts/feeds"]
        );
        let above = [
            link("StoriesController", true, None),
            link("Admin::BaseController", true, Some(&limited)),
            link("ApplicationController", true, None),
        ];
        assert_eq!(
            names(&layouts_of(&above, &exists)),
            ["layouts/admin", "layouts/application"]
        );

        assert_eq!(
            layouts_of(&[link("ApiController", true, Some(&none))], &exists),
            Layouts::default()
        );
        let sometimes = [
            link("FeedsController", true, Some(&none_limited)),
            link("ApplicationController", true, None),
        ];
        assert_eq!(names(&layouts_of(&sometimes, &exists)), ["layouts/feeds"]);
        // `layout nil` puts the class's own name first again, and `super` is still the parent's.
        let back = [
            link("FeedsController", true, Some(&reset)),
            link("ApplicationController", true, Some(&admin)),
        ];
        assert_eq!(names(&layouts_of(&back, &exists)), ["layouts/feeds"]);
        let up = [
            link("StoriesController", true, Some(&reset)),
            link("ApplicationController", true, Some(&admin)),
        ];
        assert_eq!(names(&layouts_of(&up, &exists)), ["layouts/admin"]);

        let chosen = layouts_of(&[link("PagesController", true, Some(&dynamic))], &exists);
        assert!(chosen.any && chosen.named.is_empty());
        assert!(chosen.includes("layouts/anything"));
    }

    #[test]
    fn a_layout_template_sits_in_a_directory_a_layout_name_is_looked_up_in() {
        assert!(is_layout("layouts/application"));
        assert!(is_layout("layouts/mailer/base"));
        assert!(is_layout("spree/layouts/admin"));
        assert!(!is_layout("stories/show"));
        assert!(!is_layout("application"));
        assert!(!is_layout("my_layouts/show"));
        assert!(!is_layout("mylayouts/show"));
    }
}
