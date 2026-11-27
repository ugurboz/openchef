//! The command under verification in `tests/verify.rs`, so the tests need no shell
//! utilities and behave the same on every OS. `OPENCHEF_VERIFY_HELPER` picks the mode.
#![allow(
    clippy::print_stdout,
    clippy::print_stderr,
    clippy::unwrap_used,
    reason = "test fixture"
)]

use std::io::{Read as _, Write as _};
use std::process::{Command, ExitCode};
use std::time::Duration;

fn main() -> ExitCode {
    let mode = std::env::var("OPENCHEF_VERIFY_HELPER").unwrap_or_default();
    let (name, arg) = mode.split_once(':').unwrap_or((mode.as_str(), ""));
    match name {
        "exit" => {
            let n: u8 = arg.parse().unwrap_or(0);
            println!("exiting with {n}");
            ExitCode::from(n)
        }
        "spam" => {
            let n: usize = arg.parse().unwrap_or(0);
            // Order is only guaranteed within one stream, so the markers share stdout.
            let mut out = std::io::stdout().lock();
            let line = format!("{}\n", "x".repeat(99));
            writeln!(out, "BEGIN").unwrap();
            for _ in 0..n / 100 {
                out.write_all(line.as_bytes()).unwrap();
            }
            writeln!(out, "END").unwrap();
            ExitCode::from(1)
        }
        "stderr" => {
            eprintln!("on stderr");
            ExitCode::from(1)
        }
        "sleep" => {
            std::thread::sleep(Duration::from_secs(3600));
            ExitCode::SUCCESS
        }
        "tree" => {
            let exe = std::env::current_exe().unwrap();
            let Ok(child) = Command::new(exe)
                .env("OPENCHEF_VERIFY_HELPER", "sleep")
                .spawn()
            else {
                return ExitCode::from(3);
            };
            std::fs::write(arg, child.id().to_string()).unwrap();
            std::thread::sleep(Duration::from_secs(3600));
            ExitCode::SUCCESS
        }
        "env" => {
            for (k, v) in std::env::vars_os() {
                println!("{}={}", k.to_string_lossy(), v.to_string_lossy());
            }
            ExitCode::SUCCESS
        }
        "stdin" => {
            let mut sink = Vec::new();
            drop(std::io::stdin().read_to_end(&mut sink));
            ExitCode::SUCCESS
        }
        _ => ExitCode::from(2),
    }
}
