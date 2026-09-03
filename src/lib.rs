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

/// The log filter the binary falls back to when `YA_LSP_LOG` says nothing.
///
/// Named rather than written inline in `main` because it is documented somewhere else: it is
/// what `ya-lsp.logLevel` defaults to in the VS Code manifest, which spent two releases saying
/// `warn` instead. `tests/vscode_manifest.rs` holds the two together.
pub const DEFAULT_LOG_FILTER: &str = "info";

#[cfg_attr(coverage_nightly, coverage(off))]
#[cfg(test)]
pub(crate) mod testing;
