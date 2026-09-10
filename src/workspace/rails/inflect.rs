//! Rails' inflector, minus everything this corpus did not need.
//!
//! Four functions and the two tables in [`super`] that make them irregular. What is missing
//! costs a **miss**, never a wrong answer, and that is the whole design: a class whose plural
//! names no table declares nothing, and a table no class claims declares nothing. Both
//! directions are here because both are needed — [`table_of`] goes class to table, which is the
//! safe direction the schema reader takes, and [`singularize`] goes the other way because a
//! `has_many :comments` says its element type in the plural and nowhere else.
//!
//! No acronym table, deliberately: `Api` where an application configured `API` is a name the
//! graph will not hold, which answers nothing — the same outcome as declining, reached without
//! a table of somebody else's configuration.

use super::{IRREGULAR, UNCOUNTABLE};

/// `user_sessions` -> `UserSessions`, the way Rails' inflector does it minus the acronym table.
///
/// `None` for anything that cannot be a constant. Acronyms are deliberately absent: `Api` where
/// an application configured `API` is a name the graph will not hold, which answers nothing —
/// the same outcome as declining, reached without a table of somebody else's configuration.
///
/// Public because the name guess spells a class the same way — `@user_session` is a
/// `UserSession` by the same rule that makes `user_sessions/` a `UserSessionsController`, and
/// two copies of an inflector are two inflectors that disagree.
#[must_use]
pub fn camelize(segment: &str) -> Option<String> {
    let mut name = String::with_capacity(segment.len());
    for part in segment.split('_').filter(|part| !part.is_empty()) {
        let mut characters = part.chars();
        let first = characters.next()?;
        name.extend(first.to_uppercase());
        name.push_str(characters.as_str());
    }
    // Ruby's own rule, and the whole of the validation: a constant starts with an ASCII capital.
    name.starts_with(|first: char| first.is_ascii_uppercase())
        .then_some(name)
}

/// The module a `helper :accounts` names, by Rails' own rule.
///
/// `AbstractController::Helpers::ClassMethods#modules_for_helpers` does
/// `"#{arg.to_s.camelize}Helper".constantize`, and ActiveSupport's `camelize` turns a `/` into a
/// `::` — so `helper "spree/admin/orders"` is `Spree::Admin::OrdersHelper`. No corpus writes the
/// slash form and it is read anyway, for `table_name_suffix`'s reason: it is the same walk, and
/// the only way this could name a module that exists and is not the one Rails meant is by
/// declining to read a separator Rails reads.
///
/// `None` for a segment that cannot spell a constant, which [`camelize`] already decides.
#[must_use]
pub fn helper_module(name: &str) -> Option<String> {
    let mut spelled = String::with_capacity(name.len() + 6);
    for segment in name.split('/') {
        if !spelled.is_empty() {
            spelled.push_str("::");
        }
        spelled.push_str(&camelize(segment)?);
    }
    // `split` always yields at least one segment, and a segment that cannot spell a constant has
    // already declined the whole name through `?` — so there is nothing left to test here, and
    // `helper ""` is `camelize("")` answering `None`.
    Some(spelled + "Helper")
}

/// The table a top-level class reads, by Rails' own rule: underscore it, then pluralize it.
///
/// `None` for anything that cannot be a class name. The caller must have established that the
/// class is **top level** — `Admin::Setting`'s table depends on `table_name_prefix`, which is
/// Ruby that only runs, so a namespaced model is declined here and left to
/// [`table_name_overrides`], which is the escape that works for it.
#[must_use]
pub fn table_of(class: &str) -> Option<String> {
    Some(pluralize(&underscore(class)?))
}

/// `comments` -> `comment`, by the inverse of the rules [`pluralize`] applies.
///
/// The direction the schema reader refuses, and it is admitted here because there is no other: a
/// `has_many :comments` says its element type in the plural and nowhere else. What makes it
/// safe is the same clause that makes the rest of this file safe — a name this gets wrong is a
/// class the application does not define, and a class the application does not define declares
/// nothing.
pub(super) fn singularize(word: &str) -> String {
    let (prefix, last) = word.split_at(word.rfind('_').map_or(0, |at| at + 1));
    if UNCOUNTABLE.contains(&last) {
        return word.to_owned();
    }
    if let Some((singular, _)) = IRREGULAR.iter().find(|(_, plural)| *plural == last) {
        return format!("{prefix}{singular}");
    }
    if let Some(stem) = word.strip_suffix("ies")
        && stem.ends_with(is_consonant)
    {
        return format!("{stem}y");
    }
    if let Some(stem) = word.strip_suffix("ves") {
        // The two arms `pluralize` wrote, read back: `shelves` came from a `f` and `knives`
        // from a `fe`.
        return if stem.ends_with('l') || stem.ends_with('r') {
            format!("{stem}f")
        } else {
            format!("{stem}fe")
        };
    }
    if let Some(stem) = word.strip_suffix("es")
        && ["x", "ch", "ss", "sh"]
            .iter()
            .any(|end| stem.ends_with(end))
    {
        return stem.to_owned();
    }
    word.strip_suffix('s').unwrap_or(word).to_owned()
}

/// `UserSession` -> `user_session`, the inverse of [`camelize`] and Rails' `underscore` minus
/// its acronym table, for the same reason [`camelize`] is missing one.
///
/// `pub(super)` for a second caller: `isolate_namespace Spree` installs the prefix
/// `generate_railtie_name` spells, which is `underscore(mod.name).tr("/", "_")` — one segment at
/// a time here, because this deliberately knows nothing about `::`.
pub(super) fn underscore(class: &str) -> Option<String> {
    if !class.starts_with(|first: char| first.is_ascii_uppercase()) {
        return None;
    }
    let characters: Vec<char> = class.chars().collect();
    let mut name = String::with_capacity(class.len() + 4);
    for (index, character) in characters.iter().enumerate() {
        // Rails' two rules in one: a capital after a lower-case letter or a digit starts a word,
        // and so does the last capital of a run that is followed by a lower-case one — which is
        // what makes `APIKey` into `api_key` rather than `a_p_i_key`.
        if index > 0
            && character.is_ascii_uppercase()
            && (!characters[index - 1].is_ascii_uppercase()
                || characters
                    .get(index + 1)
                    .is_some_and(char::is_ascii_lowercase))
        {
            name.push('_');
        }
        name.extend(character.to_lowercase());
    }
    Some(name)
}

/// `user_session` -> `user_sessions`, by the subset of Rails' inflector this corpus needed.
///
/// The rules are Rails' own, in Rails' own order — the later a rule is defined there the
/// earlier it is tried — minus the ones no application in the corpus exercises. What is missing
/// costs a *miss*, never a wrong answer: an unpluralizable class names a table that does not
/// exist, and a table nobody claims declares nothing.
///
/// [`super::enums`] is the second caller and it inflects an *attribute* rather than a class:
/// `enum :status` installs `self.statuses`, which is `name.pluralize` in Rails' own words. The
/// failure direction is the same one — a word this does not know installs a class method under
/// a name nobody calls, which is a member that is never looked up rather than a wrong answer.
pub(super) fn pluralize(word: &str) -> String {
    // The inflection is on the last word only: `user_session` is a session, and `admin_person`
    // is `admin_people`.
    let (prefix, last) = word.split_at(word.rfind('_').map_or(0, |at| at + 1));
    if UNCOUNTABLE.contains(&last) {
        return word.to_owned();
    }
    if let Some((_, plural)) = IRREGULAR.iter().find(|(singular, _)| *singular == last) {
        return format!("{prefix}{plural}");
    }
    if let Some(stem) = word.strip_suffix("sis") {
        return format!("{stem}ses");
    }
    if ["x", "ch", "ss", "sh"]
        .iter()
        .any(|end| word.ends_with(end))
    {
        return format!("{word}es");
    }
    if let Some(stem) = word.strip_suffix('y')
        && stem.ends_with(is_consonant)
    {
        return format!("{stem}ies");
    }
    if let Some(stem) = word.strip_suffix("fe")
        && !stem.ends_with('f')
    {
        return format!("{stem}ves");
    }
    if let Some(stem) = word.strip_suffix('f')
        && (stem.ends_with('l') || stem.ends_with('r'))
    {
        return format!("{stem}ves");
    }
    // Already plural, as far as this can tell. Rails' own `/s$/ -> "s"`.
    if word.ends_with('s') {
        return word.to_owned();
    }
    format!("{word}s")
}

/// Rails' `[^aeiouy]`, narrowed to the letters it can only have meant.
///
/// Spelled out rather than derived, because "not a vowel" is true of a digit and of every
/// letter with an accent, and `A1y` pluralizing to `a1ies` is a rule nobody wrote down.
fn is_consonant(character: char) -> bool {
    "bcdfghjklmnpqrstvwxz".contains(character)
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::*;

    /// What `helper :accounts` names, and the separator Rails reads that no corpus writes.
    ///
    /// A table rather than a test each, because the row that matters is the slash: ActiveSupport's
    /// `camelize` turns it into a `::`, and declining to read it is the only way this could name a
    /// module that exists and is not the one Rails meant.
    #[test]
    fn the_module_a_helper_call_names() {
        let rows = [
            ("accounts", Some("AccountsHelper")),
            ("frontend_urls", Some("FrontendUrlsHelper")),
            ("spree/admin/orders", Some("Spree::Admin::OrdersHelper")),
            // A segment that cannot spell a constant declines the whole name.
            ("123", None),
            ("spree//orders", None),
            ("", None),
        ];
        let answers: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(name, _)| (*name, helper_module(name)))
            .collect();
        let expected: Vec<(&str, Option<String>)> = rows
            .iter()
            .map(|(name, spelled)| (*name, spelled.map(str::to_owned)))
            .collect();
        assert_eq!(answers, expected);
    }

    /// The class→table direction, and what it declines.
    ///
    /// A table rather than a test each, because the rows that matter are the ones that answer
    /// `None` or answer something unexpected — and every miss here costs an answer that is not
    /// given, never one that is wrong.
    #[test]
    fn the_table_a_class_reads() {
        let rows: Vec<(&str, Option<String>)> = [
            "Story",
            "UserSession",
            "Category",
            "APIKey",
            // Rails' irregulars, and its uncountables.
            "Person",
            "Child",
            "Status",
            "Bus",
            "News",
            "AdminPerson",
            // The regular rules, one row each.
            "Analysis",
            "Box",
            "Church",
            "Class",
            "Wish",
            "Shelf",
            "Scarf",
            "Knife",
            // `f` and `fe` that Rails' rule deliberately leaves alone.
            "Leaf",
            "Giraffe",
            // Already plural, by Rails' own `/s$/ -> "s"`.
            "Gas",
            // Not a constant at all.
            "lowercase",
            "",
        ]
        .into_iter()
        .map(|class| (class, table_of(class)))
        .collect();

        let expected: Vec<(&str, Option<String>)> = [
            ("Story", Some("stories")),
            ("UserSession", Some("user_sessions")),
            ("Category", Some("categories")),
            ("APIKey", Some("api_keys")),
            ("Person", Some("people")),
            ("Child", Some("children")),
            ("Status", Some("statuses")),
            ("Bus", Some("buses")),
            ("News", Some("news")),
            ("AdminPerson", Some("admin_people")),
            ("Analysis", Some("analyses")),
            ("Box", Some("boxes")),
            ("Church", Some("churches")),
            ("Class", Some("classes")),
            ("Wish", Some("wishes")),
            ("Shelf", Some("shelves")),
            ("Scarf", Some("scarves")),
            ("Knife", Some("knives")),
            ("Leaf", Some("leafs")),
            ("Giraffe", Some("giraffes")),
            ("Gas", Some("gas")),
            ("lowercase", None),
            ("", None),
        ]
        .into_iter()
        .map(|(class, table)| (class, table.map(str::to_owned)))
        .collect();

        assert_eq!(rows, expected);
    }

    #[test]
    fn plurals_and_back_again() {
        // Every rule in `pluralize`, read in reverse, plus the ones only `singularize` has.
        for (plural, singular) in [
            ("comments", "comment"),
            ("stories", "story"),
            ("people", "person"),
            ("statuses", "status"),
            ("buses", "bus"),
            ("boxes", "box"),
            ("classes", "class"),
            ("dishes", "dish"),
            ("matches", "match"),
            ("houses", "house"),
            ("shelves", "shelf"),
            ("scarves", "scarf"),
            ("knives", "knife"),
            ("sheep", "sheep"),
            ("user_sessions", "user_session"),
            ("admin_people", "admin_person"),
            ("gas", "ga"),
            // Rails' own rule is `([^aeiouy]|qu)ies$`, so a vowel before the `ies` is not one:
            // the word falls through to stripping the `s`, which is a miss and not a wrong
            // answer — and a miss is a table nobody claims.
            ("aies", "aie"),
        ] {
            assert_eq!(singularize(plural), singular, "{plural}");
        }
    }
}
