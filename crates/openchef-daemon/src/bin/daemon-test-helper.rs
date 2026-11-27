//! A real daemon for `start_detached` tests: serves the home given as the first
//! argument until stopped.

use std::collections::HashMap;
use std::process::ExitCode;

use openchef_config::Paths;
use openchef_daemon::{Options, run};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> ExitCode {
    let Some(home) = std::env::args_os().nth(1) else {
        return ExitCode::from(2);
    };
    let Ok(paths) = Paths::in_home(std::path::Path::new(&home)) else {
        return ExitCode::from(2);
    };
    let options = Options {
        paths,
        version: "test".to_owned(),
        methods: HashMap::new(),
        on_stop: None,
        shutdown: CancellationToken::new(),
    };
    match run(options).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            use std::io::Write as _;
            let _logged = writeln!(std::io::stderr(), "{e}");
            ExitCode::FAILURE
        }
    }
}
