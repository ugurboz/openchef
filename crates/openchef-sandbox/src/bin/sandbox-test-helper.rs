//! Test helper: `sandbox-test-helper __sandbox --policy <json> -- CMD…` behaves like
//! `openchef __sandbox` (Linux), so integration tests confine real commands.

fn main() -> std::process::ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(2).collect();
    match openchef_sandbox::run_helper(&args) {
        Ok(never) => match never {},
        Err(e) => {
            use std::io::Write as _;
            // Nothing else can report the failure.
            let _written = writeln!(std::io::stderr(), "{e}");
            std::process::ExitCode::from(125)
        }
    }
}
