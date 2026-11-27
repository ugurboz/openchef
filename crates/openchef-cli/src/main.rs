//! The `openchef` binary.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut out = std::io::stdout().lock();
    match writeln!(out, "openchef {}", env!("CARGO_PKG_VERSION")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
