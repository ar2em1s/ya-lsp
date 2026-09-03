//! ya-lsp entry point.

use std::process::ExitCode;

const USAGE: &str = "\
ya-lsp — a standalone Ruby language server

USAGE:
    ya-lsp [--stdio]

OPTIONS:
    --stdio        Communicate over stdin/stdout (the default, and currently the only transport).
    --licenses     Print this binary's licence and the third-party notices it must carry.
    -V, --version  Print version information.
    -h, --help     Print this message.

ENVIRONMENT:
    YA_LSP_LOG     Log filter, e.g. `info`, `debug`, `ya_lsp=trace`. Logs go to stderr, never
                   to stdout, which carries the LSP transport.
";

fn main() -> ExitCode {
    let mut stdio = false;

    for argument in std::env::args().skip(1) {
        match argument.as_str() {
            "--stdio" => stdio = true,
            "-V" | "--version" => {
                println!("ya-lsp {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            // The binary embeds the `rbs` gem's signatures, whose licence requires its notice to
            // accompany a binary distribution. A bare binary — attached to a release, rehosted,
            // installed with `cargo install` — has nothing accompanying it, so it carries the
            // notice itself. See `ya_lsp::licenses`.
            "--licenses" => {
                println!("{}", ya_lsp::licenses::text());
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("ya-lsp: unrecognised argument `{other}`\n\n{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }

    // Editors overwhelmingly pass `--stdio`, but some launch the binary bare. stdio is the only
    // transport, so treat its absence as the default rather than an error.
    let _ = stdio;

    init_logging();

    match ya_lsp::server::run_stdio() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            eprintln!("ya-lsp: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Logging goes to stderr only. Anything written to stdout corrupts the LSP stream, and the
/// failure mode — the editor silently disconnecting — is miserable to debug.
fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_env("YA_LSP_LOG")
        .unwrap_or_else(|_| EnvFilter::new(ya_lsp::DEFAULT_LOG_FILTER));

    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false)
        .init();
}
