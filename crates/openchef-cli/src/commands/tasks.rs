//! `openchef run|status|logs|cancel|retry`, ported from Go `internal/cli/tasks.go`.

use std::io::Write;

use anyhow::Context as _;
use openchef_api::{
    Client, FindingWire, ListParams, NOTIFY_EVENT, NOTIFY_WATCH_ENDED, PipelineParams, RunParams,
    TaskDetail, TaskWire, WatchEnded, WatchParams, finished, friendly,
};
use openchef_config::Paths;
use openchef_core::{AttemptRole, AttemptStatus, TaskStatus};
use openchef_engine::{Event, EventKind};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::cli::{LogsArgs, PipelineArgs, RunArgs, StatusArgs, TaskArgs};
use crate::commands::daemon::{CLIENT_NAME, client_runtime, launch, report};
use crate::output::table;
use crate::render::{Renderer, first_line, short_sha, since};
use crate::{Deps, exit};

/// A change to one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// `openchef cancel`.
    Cancel,
    /// `openchef retry`.
    Retry,
}

/// A daemon refusal, shown as its message.
fn api(e: &openchef_api::Error) -> anyhow::Error {
    anyhow::anyhow!(friendly(e))
}

/// Dials the daemon. With `deps` (commands that start work) a daemon that is not
/// running is started first.
async fn connect(deps: Option<&Deps>, paths: &Paths) -> anyhow::Result<Client> {
    match Client::dial(&paths.socket, CLIENT_NAME).await {
        Err(openchef_api::Error::NotRunning) if deps.is_some() => {}
        other => return other.map_err(Into::into),
    }
    match deps.and_then(|d| d.start_daemon.clone()) {
        Some(start) => start(paths)?,
        None => drop(launch(paths).await?),
    }
    Ok(Client::dial(&paths.socket, CLIENT_NAME).await?)
}

pub(crate) fn run(
    out: &mut dyn Write,
    deps: &Deps,
    paths: &Paths,
    args: &RunArgs,
) -> anyhow::Result<i32> {
    start(out, deps, paths, args, None)
}

/// `openchef pipeline`: like `run`, but the task plans first and stops for approval
/// (or, with `--yes`, runs on).
pub(crate) fn pipeline(
    out: &mut dyn Write,
    deps: &Deps,
    paths: &Paths,
    args: &PipelineArgs,
) -> anyhow::Result<i32> {
    start(out, deps, paths, &args.run, Some(args.yes))
}

/// Starts a run (`pipeline` is `None`) or a pipeline and follows it.
fn start(
    out: &mut dyn Write,
    deps: &Deps,
    paths: &Paths,
    args: &RunArgs,
    pipeline: Option<bool>,
) -> anyhow::Result<i32> {
    let dir = match &deps.cwd {
        Some(d) => d.clone(),
        None => std::env::current_dir().context("current directory")?,
    };
    let dir = std::path::absolute(&dir).context("current directory")?;
    client_runtime()?.block_on(async {
        let mut c = connect(Some(deps), paths).await?;
        let params = RunParams {
            prompt: args.prompt.join(" "),
            dir: dir.to_string_lossy().into_owned(),
            verify: args.verify.clone(),
            agent: args.agent.clone(),
            permission_mode: args.permission_mode.clone(),
            sandbox: args.sandbox,
        };
        let started = match pipeline {
            None => c.run(&params).await,
            Some(yes) => c.pipeline(&PipelineParams { run: params, yes }).await,
        };
        let t = match started {
            Ok(t) => t,
            Err(e) => {
                c.close();
                return Err(api(&e));
            }
        };
        if args.detach {
            c.close();
            let text = format!(
                "task {} started; follow it with: openchef logs {} -f",
                t.id, t.id
            );
            report(out, args.json, &t, &text)?;
            return Ok(exit::OK);
        }
        if !args.json {
            writeln!(out, "task {} on branch openchef/{}", t.id, t.id)?;
        }
        let through_approval = pipeline == Some(true);
        let code = {
            let mut r = Renderer::new(out, args.json, args.quiet);
            follow(&mut c, &t.id, &mut r, through_approval).await
        };
        if pipeline.is_some() && !args.json && code.is_ok() {
            let shown = c.plan(&t.id).await;
            if let Ok(p) = shown
                && p.task.status == TaskStatus::AwaitingApproval.as_str()
            {
                out.write_all(crate::commands::plans::text(&p).as_bytes())?;
            }
        }
        c.close();
        code
    })
}

/// Renders a task's past and live events until it finishes or parks, the daemon goes,
/// or Ctrl-C detaches. Returns the exit code: failure when the task did not succeed.
async fn follow(
    c: &mut Client,
    id: &str,
    r: &mut Renderer<'_>,
    through_approval: bool,
) -> anyhow::Result<i32> {
    let res = c
        .watch(&WatchParams {
            id: id.to_owned(),
            replay: true,
        })
        .await
        .map_err(|e| api(&e))?;
    if res.omitted > 0 {
        r.line(&format!("  · {} earlier events not shown", res.omitted))?;
    }
    for ev in &res.events {
        r.event(ev)?;
    }
    // `--yes` approves the plan right after it is stored: keep following through it.
    let stops = |status: &str| {
        finished(status) && !(through_approval && status == TaskStatus::AwaitingApproval.as_str())
    };
    if res.finished && stops(&res.status) {
        r.finish()?;
        return Ok(outcome(&res.status)); // the store's status, not the last logged event
    }
    loop {
        let note = tokio::select! {
            n = c.notification() => n,
            _ = tokio::signal::ctrl_c() => {
                r.line(&format!(
                    "detached; task {id} keeps running (follow: openchef logs {id} -f, stop: openchef cancel {id})"
                ))?;
                return Ok(exit::OK);
            }
        };
        let Some(n) = note else {
            r.finish()?;
            anyhow::bail!(
                "the daemon closed the connection (it may have stopped); the task resumes when it restarts"
            );
        };
        match n.method.as_str() {
            NOTIFY_EVENT => {
                let ev: Event = serde_json::from_value(n.params).context("decode event")?;
                r.event(&ev)?;
                if ev.kind == EventKind::TaskStatus
                    && let Some(status) = ev.status.as_deref()
                    && stops(status)
                {
                    r.finish()?;
                    return Ok(outcome(status));
                }
            }
            NOTIFY_WATCH_ENDED => {
                r.finish()?;
                let reason = serde_json::from_value::<WatchEnded>(n.params)
                    .map_or_else(|_| "the daemon ended the watch".to_owned(), |w| w.reason);
                anyhow::bail!("stopped following task {id}: {reason}");
            }
            _ => {} // notifications of later API versions
        }
    }
}

/// The exit code for a task that stopped producing events in `status`.
fn outcome(status: &str) -> i32 {
    let ok = [
        TaskStatus::Done,
        TaskStatus::WaitingQuota,
        TaskStatus::Running,
        TaskStatus::AwaitingApproval,
    ]
    .iter()
    .any(|s| s.as_str() == status);
    if ok { exit::OK } else { exit::FAILURE }
}

pub(crate) fn logs(out: &mut dyn Write, paths: &Paths, args: &LogsArgs) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let mut c = connect(None, paths).await?;
        let mut r = Renderer::new(out, args.json, false);
        if args.follow {
            let code = follow(&mut c, &args.task, &mut r, false).await;
            c.close();
            return code;
        }
        let params = WatchParams {
            id: args.task.clone(),
            replay: true,
        };
        let res = match c.watch(&params).await {
            Ok(res) => res,
            Err(e) => {
                c.close();
                return Err(api(&e));
            }
        };
        let unwatched = c.unwatch(&args.task).await;
        c.close();
        unwatched?;
        if res.omitted > 0 {
            r.line(&format!("  · {} earlier events not shown", res.omitted))?;
        }
        for ev in &res.events {
            r.event(ev)?;
        }
        r.finish()?;
        Ok(exit::OK)
    })
}

pub(crate) fn status(out: &mut dyn Write, paths: &Paths, args: &StatusArgs) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let c = connect(None, paths).await?;
        let result = match &args.task {
            Some(id) => c.get(id).await.map(Ok),
            None => c
                .list(&ListParams {
                    active: !args.all,
                    limit: Some(args.limit),
                })
                .await
                .map(Err),
        };
        c.close();
        match result.map_err(|e| api(&e))? {
            Ok(detail) if args.json => report(out, true, &detail, "")?,
            Ok(detail) => write_detail(out, &detail)?,
            Err(tasks) if args.json => report(out, true, &tasks, "")?,
            Err(tasks) => {
                out.write_all(list_text(&tasks, args.all, OffsetDateTime::now_utc()).as_bytes())?;
            }
        }
        Ok(exit::OK)
    })
}

fn list_text(tasks: &[TaskWire], all: bool, now: OffsetDateTime) -> String {
    if tasks.is_empty() {
        return if all {
            "no tasks yet (start one with: openchef run \"<prompt>\")\n".to_owned()
        } else {
            "no active tasks (openchef status --all shows finished ones)\n".to_owned()
        };
    }
    let mut rows = vec![vec![
        "TASK".to_owned(),
        "STATUS".into(),
        "STARTED".into(),
        "PROMPT".into(),
    ]];
    for t in tasks {
        let started = OffsetDateTime::parse(&t.created_at, &Rfc3339)
            .map_or_else(|_| "-".to_owned(), |at| since(at, now));
        rows.push(vec![
            t.id.clone(),
            t.status.clone(),
            started,
            first_line(&t.prompt, 60),
        ]);
    }
    table(&rows)
}

/// `15:04 UTC Jan 2`.
fn short_time(rfc3339: &str) -> String {
    let format = time::macros::format_description!(
        "[hour]:[minute] UTC [month repr:short] [day padding:none]"
    );
    OffsetDateTime::parse(rfc3339, &Rfc3339)
        .ok()
        .and_then(|t| t.to_offset(time::UtcOffset::UTC).format(&format).ok())
        .unwrap_or_else(|| rfc3339.to_owned())
}

fn write_detail(out: &mut dyn Write, d: &TaskDetail) -> std::io::Result<()> {
    let t = &d.task;
    write!(out, "task     {} ({})\nstatus   {}", t.id, t.kind, t.status)?;
    if let Some(reason) = t.reason.as_deref().filter(|r| !r.is_empty()) {
        write!(out, ": {reason}")?;
    }
    if let Some(at) = &t.resume_at {
        write!(out, " (resumes {})", short_time(at))?;
    }
    write!(out, "\nrepo     {}\n", t.repo)?;
    if let Some(branch) = &t.branch {
        writeln!(
            out,
            "branch   {branch}\nworktree {}",
            t.worktree.as_deref().unwrap_or("-")
        )?;
    }
    writeln!(out, "prompt   {}", first_line(&t.prompt, 100))?;
    if d.debate_turns > 0 {
        writeln!(
            out,
            "debate   {} of {} turns",
            d.debate_turns, d.debate_budget
        )?;
    }
    writeln!(out)?;
    // Step 0: the planner and the plan council.
    let council = latest_findings(&d.findings, 0);
    if !council.is_empty() || d.attempts.iter().any(|a| a.step == 0) {
        writeln!(out, "plan")?;
        write_attempts(out, d, 0)?;
        write_findings(out, &council)?;
    }
    for s in &d.steps {
        write!(out, "step {}  {:<8} {}", s.index, s.status, s.title)?;
        if let Some(commit) = &s.commit {
            write!(out, "  ({})", short_sha(commit))?;
        }
        writeln!(out)?;
        write_attempts(out, d, s.index)?;
        write_findings(out, &latest_findings(&d.findings, s.index))?;
    }
    Ok(())
}

/// The attempts at `step`, with their role unless they implement.
fn write_attempts(out: &mut dyn Write, d: &TaskDetail, step: u32) -> std::io::Result<()> {
    for a in d.attempts.iter().filter(|a| a.step == step) {
        write!(out, "  attempt {}  ", a.id)?;
        if a.role != AttemptRole::Implement.as_str() {
            write!(out, "{} ", a.role)?;
        }
        write!(out, "{} on {}: {}", a.agent, a.pool, a.status)?;
        if let Some(outcome) = a.outcome.as_deref().filter(|o| !o.is_empty())
            && a.status != AttemptStatus::Succeeded.as_str()
        {
            write!(out, " ({})", first_line(outcome, 80))?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// Each finding of step `step` as its latest round left it: a finding the reviewer
/// reports again in a later round keeps its id.
pub(crate) fn latest_findings(all: &[FindingWire], step: u32) -> Vec<&FindingWire> {
    let mut latest: Vec<&FindingWire> = Vec::new();
    for f in all.iter().filter(|f| f.step == step) {
        match latest.iter_mut().find(|l| l.id == f.id) {
            Some(l) if f.round >= l.round => *l = f,
            Some(_) => {}
            None => latest.push(f),
        }
    }
    latest
}

/// One finding that still needs a person: status, id, severity, place, claim and the
/// rebuttal or ruling, if any.
pub(crate) fn finding_line(f: &FindingWire) -> String {
    let place = match (&f.file, f.line, &f.contract) {
        (Some(file), Some(line), _) => format!("{file}:{line}"),
        (Some(file), None, _) => file.clone(),
        (None, _, Some(contract)) => contract.clone(),
        (None, _, None) => "-".to_owned(),
    };
    let note = f
        .note
        .as_deref()
        .filter(|n| !n.is_empty())
        .map_or_else(String::new, |n| format!(" ({})", first_line(n, 80)));
    format!(
        "{:<8} {} {:<7} {place}  {}{note}",
        f.status,
        f.id,
        f.severity,
        first_line(&f.claim, 80)
    )
}

/// The outcome of a step's review (or, for step 0, the plan council): a count per
/// status, then every finding still needing a person (open or disputed); nits are only
/// counted.
fn write_findings(out: &mut dyn Write, findings: &[&FindingWire]) -> std::io::Result<()> {
    if findings.is_empty() {
        return Ok(());
    }
    let counts: Vec<String> = ["open", "disputed", "dismissed", "fixed", "nit"]
        .into_iter()
        .filter_map(|status| {
            let n = findings.iter().filter(|f| f.status == status).count();
            (n > 0).then(|| format!("{n} {status}"))
        })
        .collect();
    writeln!(out, "  findings {}", counts.join(", "))?;
    for f in findings
        .iter()
        .filter(|f| f.status == "open" || f.status == "disputed")
    {
        writeln!(out, "    {}", finding_line(f))?;
    }
    Ok(())
}

pub(crate) fn act(
    out: &mut dyn Write,
    paths: &Paths,
    args: &TaskArgs,
    action: Action,
) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let c = connect(None, paths).await?;
        let result = match action {
            Action::Cancel => c.cancel(&args.task).await,
            Action::Retry => c.retry(&args.task).await,
        };
        c.close();
        let t = result.map_err(|e| api(&e))?;
        let text = match action {
            Action::Cancel => format!("task {} cancelled", t.id),
            Action::Retry => format!(
                "task {} restarted; follow it with: openchef logs {} -f",
                t.id, t.id
            ),
        };
        report(out, args.json, &t, &text)?;
        Ok(exit::OK)
    })
}
