#![cfg_attr(coverage_nightly, feature(coverage_attribute))]
//! ya-lsp — a standalone Ruby language server that never executes Ruby.
//!
//! Module map:
//!
//! - [`server`] — LSP lifecycle, capability negotiation, message dispatch.
//! - [`analysis`] — the analysis thread; the only place a rubydex type may be named.
//! - [`workspace`] — workspace root, `ya-lsp.toml`, file discovery, URI canonicalisation.
//! - [`logging`] — where the log goes, and the one handle that can re-point it.
//! - [`licenses`] — what `--licenses` prints, and why the binary has to carry it.
//! - [`messages`] — every sentence the server says to a user, and the rule they are written to.

pub mod analysis;
pub mod generated;
pub mod knowledge;
pub mod licenses;
pub mod logging;
pub mod messages;
pub mod server;
pub mod workspace;

/// Re-exported so the path it has always had keeps working; it lives in [`logging`] now,
/// beside the code that reads it.
pub use logging::DEFAULT_LOG_FILTER;

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
pub(crate) mod testing;
