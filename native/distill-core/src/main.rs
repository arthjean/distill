#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]
// Failure is the stable serialized engine boundary and SurfaceError carries its
// optional full artifact reference so machine-mode failures remain recoverable.
#![allow(clippy::result_large_err)]

mod cli;
mod codex;
mod mcp;
mod setup;

fn main() {
    let code = cli::run(
        std::env::args_os().skip(1).collect(),
        std::io::stdin(),
        std::io::stdout(),
        std::io::stderr(),
    );
    std::process::exit(code);
}
