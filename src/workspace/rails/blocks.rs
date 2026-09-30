//! Which Rails methods run a block or a lambda against another `self`, written as the RBS
//! `[self: T]` a signature says it with.
//!
//! A block normally runs against the `self` of the code around it. A DSL's does not:
//! `before_save do … end` is written in a class body and runs against a record,
//! `scope :recent, -> { … }` against a relation, `Rails.application.configure do … end` against the
//! application. Rails does it with `instance_exec`, `instance_eval` and `class_eval`, which no
//! signature ships for. So every receiverless call in such a block was looked up on the class body
//! the block sits in, which has none of the record's or the relation's methods.
//!
//! RBS has the words for it: `{ () [self: instance] -> void }` for a block and
//! `^() [self: instance] -> untyped` for a lambda argument. So this is a table like the rest of this
//! directory, text out, and [`Types`](crate::analysis::types::Types) reads it from the RBS the way it
//! reads Ruby's own.
//!
//! # Which methods, and which `self`
//!
//! Read off the Rails 8.1 source, where each one calls `instance_exec` or `instance_eval` on the
//! block it was handed (the scan and its list are in `tmp/bench/selfblocks/`):
//!
//! | methods | `self` inside |
//! | --- | --- |
//! | `scope`'s lambda, `default_scope`'s | the model's relation (`Relation#_exec_scope`) |
//! | a model's callbacks, `validate`, and their `if:` and `unless:` lambdas | the record |
//! | a controller's, a job's and a mailer's callbacks, `rescue_from`, `after_discard` | the object |
//! | `content_security_policy`, `permissions_policy`, `rate_limit`'s `by:` and `with:` | the controller |
//! | `queue_as`, `queue_with_priority` | the job |
//! | `initializer` | the railtie or engine |
//!
//! **`retry_on` and `discard_on` are not in it.** Their block is called with `yield self, error`,
//! which leaves `self` alone: the class body's.
//!
//! **`instance` is the receiver's instance**, as RBS reads it: `before_save` is declared on a base
//! class and runs against an instance of the subclass it is written in.

use crate::generated::COLLECTION;

/// A callback's or a validation's `if:` and `unless:` lambdas, which `ActiveSupport::Callbacks`
/// runs against the object the callback runs on.
pub(super) const CONDITIONS: &str =
    "?if: ^() [self: instance] -> untyped, ?unless: ^() [self: instance] -> untyped";

/// A callback registrar's parameters: names, a condition hash and a block, all run against the
/// object.
fn callback() -> String {
    format!("(*untyped, {CONDITIONS}, **untyped) ?{{ (untyped) [self: instance] -> void }}")
}

/// The parameters a gem `module ClassMethods` member is written with here, where its block or a
/// lambda it takes runs against something else, instead of the `untyped` ones its `def` implies.
///
/// Keyed by the concern that puts the module on its includers and the method's name, as
/// [`super::concerns::declare`] knows them. `None` for every other member, which keeps its own.
#[must_use]
pub(super) fn class_method(concern: &str, method: &str) -> Option<String> {
    let rebound = match (concern, method) {
        ("ActiveRecord::Scoping::Named", "scope") => {
            format!("(untyped, ^() [self: {COLLECTION}] -> untyped) ?{{ (*untyped) -> untyped }}")
        }
        ("ActiveModel::Validations", "validate") => callback(),
        ("ActiveModel::Validations", "validates" | "validates_with") => {
            format!("(*untyped, {CONDITIONS}, **untyped)")
        }
        ("ActiveSupport::Rescuable", "rescue_from") => "(*untyped, ?with: ^() [self: instance] -> \
                                                        untyped) ?{ (untyped) [self: instance] -> \
                                                        untyped }"
            .to_owned(),
        (
            "ActiveJob::Callbacks",
            "before_enqueue" | "after_enqueue" | "around_enqueue" | "before_perform"
            | "after_perform" | "around_perform",
        )
        | ("ActionMailer::Callbacks", "before_deliver" | "after_deliver" | "around_deliver") => {
            callback()
        }
        ("ActiveJob::Exceptions", "after_discard") => {
            "() { (untyped, untyped) [self: instance] -> void }".to_owned()
        }
        ("ActiveJob::QueueName", "queue_as")
        | ("ActiveJob::QueuePriority", "queue_with_priority") => {
            "(?untyped) ?{ () [self: instance] -> untyped }".to_owned()
        }
        ("ActionController::ContentSecurityPolicy", "content_security_policy") => {
            "(?untyped, **untyped) ?{ (untyped) [self: instance] -> void }".to_owned()
        }
        ("ActionController::PermissionsPolicy", "permissions_policy") => {
            "(**untyped) ?{ (untyped) [self: instance] -> void }".to_owned()
        }
        ("ActionController::RateLimiting", "rate_limit") => {
            "(to: untyped, within: untyped, ?by: ^() [self: instance] -> untyped, ?with: ^() \
             [self: instance] -> untyped, **untyped)"
                .to_owned()
        }
        ("Rails::Initializable", "initializer") => {
            "(untyped, ?untyped) { (untyped) [self: instance] -> void }".to_owned()
        }
        _ => return None,
    };
    Some(rebound)
}

/// A model callback's parameters ([`super::relations`]' twenty-three): the block is handed the
/// record, as before, and now runs against it too, as do its `if:` and `unless:` lambdas.
#[must_use]
pub(super) fn model_callback(element: &str) -> String {
    format!("(*untyped, {CONDITIONS}, **untyped) ?{{ ({element}) [self: instance] -> void }}")
}

/// The controller callbacks `AbstractController::Callbacks` makes with `define_method`, which no
/// file declares: every prefix of `before`, `after` and `around`. The `skip_` ones take no block.
pub(super) const CONTROLLER_CALLBACKS: [&str; 9] = [
    "before_action",
    "after_action",
    "around_action",
    "prepend_before_action",
    "prepend_after_action",
    "prepend_around_action",
    "append_before_action",
    "append_after_action",
    "append_around_action",
];

/// A controller callback's parameters: [`callback`]'s.
#[must_use]
pub(super) fn controller_callback() -> String {
    callback()
}

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
mod tests {
    use super::{CONTROLLER_CALLBACKS, class_method, controller_callback, model_callback};

    #[test]
    fn a_member_nothing_rebinds_keeps_its_own_parameters() {
        assert_eq!(
            class_method("ActiveRecord::Scoping::Named", "unscoped"),
            None
        );
        assert_eq!(class_method("Tallyable", "scope"), None);
    }

    #[test]
    fn every_rebinding_writes_the_self_it_runs_against() {
        let rows = [
            ("ActiveRecord::Scoping::Named", "scope"),
            ("ActiveModel::Validations", "validate"),
            ("ActiveModel::Validations", "validates"),
            ("ActiveModel::Validations", "validates_with"),
            ("ActiveSupport::Rescuable", "rescue_from"),
            ("ActiveJob::Callbacks", "before_perform"),
            ("ActionMailer::Callbacks", "after_deliver"),
            ("ActiveJob::Exceptions", "after_discard"),
            ("ActiveJob::QueueName", "queue_as"),
            ("ActiveJob::QueuePriority", "queue_with_priority"),
            (
                "ActionController::ContentSecurityPolicy",
                "content_security_policy",
            ),
            ("ActionController::PermissionsPolicy", "permissions_policy"),
            ("ActionController::RateLimiting", "rate_limit"),
            ("Rails::Initializable", "initializer"),
        ];
        for (concern, method) in rows {
            let written = class_method(concern, method).expect("a row");
            assert!(written.contains("[self: "), "{concern}.{method}: {written}");
        }
        assert!(model_callback("Story").contains("(Story) [self: instance]"));
        assert!(controller_callback().contains("?if: ^() [self: instance]"));
        assert_eq!(CONTROLLER_CALLBACKS.len(), 9);
    }
}
