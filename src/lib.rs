#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! ya-lsp — a standalone Ruby language server that never executes Ruby.
//!
//! Module map:
//!
//! - [`server`] — LSP lifecycle, capability negotiation, message dispatch.
//! - [`analysis`] — the analysis thread; the only place a rubydex type may be named.
//! - [`workspace`] — workspace root, `ya-lsp.toml`, file discovery, URI canonicalisation.
//! - [`licenses`] — what `--licenses` prints, and why the binary has to carry it.
//! - [`messages`] — every sentence the server says to a user, and the rule they are written to.

pub mod analysis;
pub mod licenses;
pub mod messages;
pub mod server;
pub mod workspace;

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
pub(crate) mod testing;
