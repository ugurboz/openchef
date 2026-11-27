//! One module per command.

pub(crate) mod daemon;
pub(crate) mod doctor;
pub(crate) mod init;
pub(crate) mod tasks;
pub(crate) mod version;

use std::io::Write;

use clap::CommandFactory as _;

use crate::cli::{Cli, Command, PENDING};
use crate::{Deps, exit};

/// Runs the parsed command and returns its exit code.
pub(crate) fn dispatch(
    cli: Cli,
    out: &mut dyn Write,
    err: &mut dyn Write,
    deps: &Deps,
) -> anyhow::Result<i32> {
    match cli.command {
        // Until the TUI exists (R6), running without a command prints help.
        None => {
            write!(out, "{}", Cli::command().render_long_help())?;
            Ok(exit::OK)
        }
        Some(Command::Version { json }) => version::run(out, json),
        Some(Command::Doctor { json }) => {
            doctor::run(out, deps, &deps.paths(cli.home.as_ref())?, json)
        }
        Some(Command::Init(args)) => init::run(out, deps, &deps.paths(cli.home.as_ref())?, &args),
        Some(Command::Daemon { command }) => {
            daemon::run(out, &deps.paths(cli.home.as_ref())?, &command)
        }
        Some(Command::Run(args)) => tasks::run(out, deps, &deps.paths(cli.home.as_ref())?, &args),
        Some(Command::Status(args)) => tasks::status(out, &deps.paths(cli.home.as_ref())?, &args),
        Some(Command::Logs(args)) => tasks::logs(out, &deps.paths(cli.home.as_ref())?, &args),
        Some(Command::Cancel(args)) => tasks::act(
            out,
            &deps.paths(cli.home.as_ref())?,
            &args,
            tasks::Action::Cancel,
        ),
        Some(Command::Retry(args)) => tasks::act(
            out,
            &deps.paths(cli.home.as_ref())?,
            &args,
            tasks::Action::Retry,
        ),
        Some(Command::Pending(words)) => {
            let name = words.first().map_or("", String::as_str);
            match PENDING.iter().find(|(n, _)| *n == name) {
                Some((_, milestone)) => {
                    writeln!(
                        err,
                        "error: `openchef {name}` is not implemented yet ({milestone})"
                    )?;
                }
                None => writeln!(
                    err,
                    "error: unknown command `{name}` (see `openchef --help`)"
                )?,
            }
            Ok(exit::USAGE)
        }
    }
}
