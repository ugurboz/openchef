//! The `openchef` binary.

use std::process::ExitCode;

fn main() -> ExitCode {
    let deps = openchef_cli::Deps::system();
    let code = openchef_cli::run(
        std::env::args_os(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        &deps,
    );
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
