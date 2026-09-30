//! ya-lsp entry point.

use std::process::ExitCode;

// Indexing a real application's bundle is thousands of files of short-lived allocation across a
// thread pool, and it is most of a cold open. Swapping the allocator under it makes that phase
// markedly faster; `Cargo.toml` says why this is mimalloc rather than the jemalloc rubydex offers.
//
// Here, not in `lib.rs`, on purpose: a `#[global_allocator]` binds the whole binary, so in the
// library it would also bind every test binary and silently move the numbers the suite and the
// coverage run are read against. The shipped server is what this is for.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _};

const USAGE: &str = "\
ya-lsp — a standalone Ruby language server

USAGE:
    ya-lsp [--stdio]
    ya-lsp coverage [DIR]

COMMANDS:
    coverage [DIR] Index the project at DIR (the current directory by default) with its gems, and
                   print what share of the calls its own code makes ya-lsp can type: 2,000 calls
                   sampled, with the sample's 95% error. Test and migration folders are left out,
                   as ya-lsp.toml's [trees] says. Progress goes to stderr, the result to stdout.

OPTIONS:
    --stdio        Communicate over stdin/stdout (the default, and currently the only transport).
    --licenses     Print this binary's licence and the third-party notices it must carry.
    -V, --version  Print version information.
    -h, --help     Print this message.

ENVIRONMENT:
    YA_LSP_LOG     Log filter, e.g. `info`, `debug`, `ya_lsp=trace`. Outranks [log] level in
                   ya-lsp.toml. Logs go to stderr, never to stdout, which carries the LSP
                   transport; [log] file adds a second copy on disk.
";

fn main() -> ExitCode {
    let mut stdio = false;
    let arguments: Vec<String> = std::env::args().skip(1).collect();

    // A command, not a server: it indexes the project itself and prints one line. Logs only where
    // YA_LSP_LOG asks for them, so progress is the whole of stderr otherwise.
    if let Some(("coverage", rest)) = arguments
        .split_first()
        .map(|(first, rest)| (first.as_str(), rest))
    {
        if let Ok(filter) = std::env::var("YA_LSP_LOG") {
            let (sinks, _) = ya_lsp::logging::install(Some(filter));
            tracing_subscriber::registry().with(sinks).init();
        }
        let terminal = std::io::IsTerminal::is_terminal(&std::io::stderr());
        let measured = ya_lsp::analysis::coverage::command(
            rest,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
            terminal,
        );
        return if measured {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        };
    }

    for argument in arguments {
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
            // accompany a binary distribution. A bare binary (attached to a release, rehosted,
            // installed with `cargo install`) has nothing accompanying it, so it carries the notice
            // itself. See `ya_lsp::licenses`.
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
    // transport, so its absence is the default, not an error.
    let _ = stdio;

    // The decisions are all in `ya_lsp::logging`; the two lines here are the edge: reading the
    // environment, and the one call that cannot be undone. Anything more in `main` is a line no
    // test reaches.
    let (sinks, reload) = ya_lsp::logging::install(std::env::var("YA_LSP_LOG").ok());
    tracing_subscriber::registry().with(sinks).init();

    match ya_lsp::server::run_stdio(reload) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!("{error:#}");
            eprintln!("ya-lsp: {error:#}");
            ExitCode::FAILURE
        }
    }
}
