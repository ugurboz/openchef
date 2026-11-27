//! `openchef daemon run|start|stop|status`.

use std::io::Write;
use std::time::Duration;

use anyhow::Context as _;
use openchef_api::Client;
use openchef_config::Paths;
use openchef_daemon::{Detached, STOP_TIMEOUT, daemon_options, start_detached};
use serde::Serialize;
use serde_json::json;
use time::OffsetDateTime;
use tokio_util::sync::CancellationToken;

use crate::cli::DaemonCommand;
use crate::commands::version::BuildInfo;
use crate::exit;

/// Bounds the wait for a background daemon to answer.
pub(crate) const START_TIMEOUT: Duration = Duration::from_secs(10);
/// Bounds `daemon stop`: the daemon's own shutdown bound plus slack.
const STOP_WAIT: Duration = Duration::from_secs(STOP_TIMEOUT.as_secs() + 5);
/// The name clients introduce themselves with.
pub(crate) const CLIENT_NAME: &str = "openchef-cli";

pub(crate) fn run(
    out: &mut dyn Write,
    paths: &Paths,
    command: &DaemonCommand,
) -> anyhow::Result<i32> {
    match command {
        DaemonCommand::Run => foreground(paths),
        DaemonCommand::Start(f) => client_runtime()?.block_on(start(out, paths, f.json)),
        DaemonCommand::Stop(f) => client_runtime()?.block_on(stop(out, paths, f.json)),
        DaemonCommand::Status(f) => client_runtime()?.block_on(status(out, paths, f.json)),
    }
}

/// A runtime for one client command: the CLI does nothing concurrently.
pub(crate) fn client_runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("start the async runtime")
}

/// The daemon itself, in the foreground (what `daemon start` launches). Interrupt and
/// terminate signals stop it gracefully.
fn foreground(paths: &Paths) -> anyhow::Result<i32> {
    // Read while this process has one thread: Unix platforms cannot tell the local
    // offset safely once the runtime's threads run. Unknown means UTC.
    let local_offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("start the async runtime")?;
    rt.block_on(async {
        let shutdown = CancellationToken::new();
        let signals = tokio::spawn(on_signal(shutdown.clone()));
        let options = daemon_options(
            paths.clone(),
            BuildInfo::current().version.to_owned(),
            shutdown,
            local_offset,
        );
        let result = openchef_daemon::run(options).await;
        signals.abort();
        result?;
        Ok(exit::OK)
    })
}

/// Cancels `shutdown` on Ctrl-C or SIGTERM.
async fn on_signal(shutdown: CancellationToken) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            // Without a SIGTERM handler Ctrl-C and `daemon stop` still work.
            if tokio::signal::ctrl_c().await.is_ok() {
                shutdown.cancel();
            }
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        // A failed handler leaves `daemon stop` as the way out.
        if tokio::signal::ctrl_c().await.is_err() {
            return;
        }
    }
    shutdown.cancel();
}

/// Launches this executable's `daemon run` in the background and waits until it
/// answers.
pub(crate) async fn launch(paths: &Paths) -> anyhow::Result<openchef_api::InitializeResult> {
    let exe = std::env::current_exe().context("locate the openchef executable")?;
    let mut args = vec!["daemon".into(), "run".into()];
    args.extend(["--home".into(), paths.home.clone().into_os_string()]);
    let detached = Detached {
        exe,
        args,
        env: None,
        log_path: paths.logs.join("daemon.log"),
        socket: paths.socket.clone(),
        timeout: START_TIMEOUT,
    };
    Ok(start_detached(&detached).await?)
}

async fn start(out: &mut dyn Write, paths: &Paths, json: bool) -> anyhow::Result<i32> {
    match Client::dial(&paths.socket, CLIENT_NAME).await {
        Ok(c) => {
            let info = c.daemon.clone();
            c.close();
            report(
                out,
                json,
                &info,
                &format!("daemon already running (pid {})", info.pid),
            )?;
            return Ok(exit::OK);
        }
        Err(openchef_api::Error::NotRunning) => {}
        Err(e) => return Err(e.into()),
    }
    let info = launch(paths).await?;
    report(
        out,
        json,
        &info,
        &format!("daemon started (pid {})", info.pid),
    )?;
    Ok(exit::OK)
}

async fn stop(out: &mut dyn Write, paths: &Paths, json: bool) -> anyhow::Result<i32> {
    let c = match Client::dial(&paths.socket, CLIENT_NAME).await {
        Ok(c) => c,
        Err(openchef_api::Error::NotRunning) => {
            report(out, json, &json!({"running": false}), "daemon not running")?;
            return Ok(exit::OK);
        }
        Err(e) => return Err(e.into()),
    };
    let pid = c.daemon.pid;
    let closed = c.closed();
    c.stop().await?;
    let stopped = tokio::time::timeout(STOP_WAIT, closed.cancelled()).await;
    c.close();
    if stopped.is_err() {
        anyhow::bail!("daemon (pid {pid}) did not stop within {STOP_WAIT:?}");
    }
    report(
        out,
        json,
        &json!({"running": false, "pid": pid}),
        &format!("daemon stopped (pid {pid})"),
    )?;
    Ok(exit::OK)
}

async fn status(out: &mut dyn Write, paths: &Paths, json: bool) -> anyhow::Result<i32> {
    let c = match Client::dial(&paths.socket, CLIENT_NAME).await {
        Ok(c) => c,
        Err(openchef_api::Error::NotRunning) => {
            report(out, json, &json!({"running": false}), "daemon not running")?;
            return Ok(exit::NOT_RUNNING);
        }
        Err(e) => return Err(e.into()),
    };
    let st = c.status().await;
    c.close();
    let st = st?;
    let up = (OffsetDateTime::now_utc() - st.started_at)
        .whole_seconds()
        .max(0);
    let text = format!(
        "daemon running (pid {}, version {}, up {})\nsocket {}",
        st.pid,
        st.daemon_version,
        uptime(up),
        st.socket.display()
    );
    report(out, json, &st, &text)?;
    Ok(exit::OK)
}

/// `1h2m3s`, like Go's rounded durations.
fn uptime(secs: i64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match (h, m) {
        (0, 0) => format!("{s}s"),
        (0, _) => format!("{m}m{s}s"),
        _ => format!("{h}h{m}m{s}s"),
    }
}

/// Prints `v` as pretty JSON or `text` as a line.
pub(crate) fn report(
    out: &mut dyn Write,
    json: bool,
    v: &impl Serialize,
    text: &str,
) -> anyhow::Result<()> {
    if json {
        serde_json::to_writer_pretty(&mut *out, v).context("encode result")?;
        writeln!(out)?;
    } else {
        writeln!(out, "{text}")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn uptime() {
        assert_eq!(super::uptime(5), "5s");
        assert_eq!(super::uptime(65), "1m5s");
        assert_eq!(super::uptime(3725), "1h2m5s");
    }
}
