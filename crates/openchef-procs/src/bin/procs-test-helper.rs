//! A child program for the `openchef-procs` tests. `OPENCHEF_PROCS_HELPER` selects the
//! mode; the tests start it under supervision and observe what happens.
#![allow(unsafe_code)]

use std::io::{self, Write as _};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

const MODE_ENV: &str = "OPENCHEF_PROCS_HELPER";
const LONG: Duration = Duration::from_mins(10);

fn main() -> ExitCode {
    let mode = std::env::var(MODE_ENV).unwrap_or_default();
    match run(&mode) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            // If stderr itself is gone there is nowhere left to report to.
            drop(writeln!(io::stderr(), "procs-test-helper {mode}: {e}"));
            ExitCode::from(2)
        }
    }
}

fn run(mode: &str) -> io::Result<()> {
    match mode {
        "echo" => io::copy(&mut io::stdin(), &mut io::stdout()).map(|_| ()),
        "until-eof" => io::copy(&mut io::stdin(), &mut io::sink()).map(|_| ()),
        "sleep" => {
            thread::sleep(LONG);
            Ok(())
        }
        "ignore-term" => {
            ignore_term();
            writeln!(io::stdout(), "ready")?;
            thread::sleep(LONG);
            Ok(())
        }
        "spawn" | "spawn-and-exit" => {
            // The grandchild stays in this process's kill domain (no new group or job).
            let exe = std::env::current_exe()?;
            let child = Command::new(exe)
                .env_clear()
                .envs(passthrough())
                .env(MODE_ENV, "sleep")
                .spawn()?;
            let token = openchef_procs::start_token(child.id()).map_err(io::Error::other)?;
            writeln!(io::stdout(), "{} {token}", child.id())?;
            if mode == "spawn" {
                thread::sleep(LONG);
            }
            Ok(())
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown mode {other:?}"),
        )),
    }
}

/// Variables a child needs to start on every OS.
fn passthrough() -> Vec<(String, String)> {
    ["SYSTEMROOT", "TMPDIR", "TEMP", "TMP"]
        .into_iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_owned(), v)))
        .collect()
}

#[cfg(unix)]
fn ignore_term() {
    // SAFETY: setting SIGTERM to SIG_IGN installs no handler code and is always sound.
    unsafe {
        let _ = libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
}

#[cfg(not(unix))]
fn ignore_term() {}
