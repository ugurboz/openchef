//! `openchef plan [approve|reject|edit] <task>` (§7.5 phase 3, D-037).

use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;

use anyhow::Context as _;
use openchef_api::{Client, EditParams, PlanResult, RejectParams, friendly};
use openchef_config::Paths;
use openchef_engine::plan::Plan;

use crate::cli::{PlanArgs, PlanCommand};
use crate::commands::daemon::{CLIENT_NAME, client_runtime, report};
use crate::exit;

fn api(e: &openchef_api::Error) -> anyhow::Error {
    anyhow::anyhow!(friendly(e))
}

pub(crate) fn run(out: &mut dyn Write, paths: &Paths, args: &PlanArgs) -> anyhow::Result<i32> {
    client_runtime()?.block_on(async {
        let c = Client::dial(&paths.socket, CLIENT_NAME).await?;
        let result = command(&c, out, args).await;
        c.close();
        result
    })
}

async fn command(c: &Client, out: &mut dyn Write, args: &PlanArgs) -> anyhow::Result<i32> {
    match &args.command {
        None => {
            let Some(task) = &args.task else {
                anyhow::bail!("name the task: openchef plan <task>");
            };
            let p = c.plan(task).await.map_err(|e| api(&e))?;
            if args.json {
                report(out, true, &p.plan, "")?;
            } else {
                out.write_all(text(&p).as_bytes())?;
            }
        }
        Some(PlanCommand::Approve { task }) => {
            let t = c.approve_plan(task).await.map_err(|e| api(&e))?;
            let msg = format!(
                "plan approved; task {} is running (follow: openchef logs {} -f)",
                t.id, t.id
            );
            report(out, false, &t, &msg)?;
        }
        Some(PlanCommand::Reject { task, reason }) => {
            let reason = (!reason.is_empty()).then(|| reason.join(" "));
            let t = c
                .reject_plan(&RejectParams {
                    id: task.clone(),
                    reason,
                })
                .await
                .map_err(|e| api(&e))?;
            report(
                out,
                false,
                &t,
                &format!("plan rejected; task {} is cancelled", t.id),
            )?;
        }
        Some(PlanCommand::Edit { task, file }) => {
            let current = c.plan(task).await.map_err(|e| api(&e))?;
            let plan = match file {
                Some(f) => read_plan(f)?,
                None => edit_in_editor(&current.plan)?,
            };
            let p = c
                .edit_plan(&EditParams {
                    id: task.clone(),
                    plan,
                })
                .await
                .map_err(|e| api(&e))?;
            out.write_all(text(&p).as_bytes())?;
        }
    }
    Ok(exit::OK)
}

fn read_plan(path: &Path) -> anyhow::Result<Plan> {
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("{} is not a plan", path.display()))
}

/// Opens the plan as JSON in `$VISUAL` or `$EDITOR` and reads it back.
fn edit_in_editor(plan: &Plan) -> anyhow::Result<Plan> {
    let editor = std::env::var_os("VISUAL")
        .or_else(|| std::env::var_os("EDITOR"))
        .filter(|e| !e.is_empty())
        .context("set $VISUAL or $EDITOR, or pass --file <plan.json>")?;
    let dir = tempfile_dir()?;
    let path = dir.join("plan.json");
    std::fs::write(&path, serde_json::to_string_pretty(plan)?)
        .with_context(|| format!("write {}", path.display()))?;
    let status = editor_command(&editor, &path)
        .status()
        .context("start the editor")?;
    anyhow::ensure!(
        status.success(),
        "the editor exited with {status}; the plan is unchanged"
    );
    let plan = read_plan(&path);
    // The draft is a copy; losing it changes nothing.
    let _removed = std::fs::remove_dir_all(&dir);
    plan
}

/// The editor value may carry arguments (`code --wait`): Unix runs it through the
/// shell with the file as a separate argument, never pasted into the command string.
#[cfg(unix)]
fn editor_command(editor: &std::ffi::OsStr, file: &Path) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    let _c = c.args(["-c", "exec $0 \"$1\""]).arg(editor).arg(file);
    c
}

/// Windows has no `sh`: the editor value is the program.
#[cfg(not(unix))]
fn editor_command(editor: &std::ffi::OsStr, file: &Path) -> std::process::Command {
    let mut c = std::process::Command::new(editor);
    let _c = c.arg(file);
    c
}

fn tempfile_dir() -> anyhow::Result<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("openchef-plan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(dir)
}

/// The plan as people read it, with the commands that act on it.
pub(crate) fn text(p: &PlanResult) -> String {
    let t = &p.task;
    let mut out = format!("plan for task {} ({}): {}\n", t.id, t.status, p.plan.title);
    if p.plan.steps.is_empty() {
        out.push_str("  (no plan yet)\n");
    }
    for (i, s) in p.plan.steps.iter().enumerate() {
        // Writing to a String cannot fail.
        let _w = writeln!(out, "  {}. {}  [{}]", i + 1, s.title, s.importance);
        let verify = s.verify.as_deref().unwrap_or("none");
        let _w = writeln!(out, "     verify: {verify}");
        if !s.files.is_empty() {
            let _w = writeln!(out, "     files: {}", s.files.join(", "));
        }
    }
    let open: Vec<&openchef_api::FindingWire> = super::tasks::latest_findings(&p.findings, 0)
        .into_iter()
        .filter(|f| f.status == "open")
        .collect();
    if !open.is_empty() {
        let _w = writeln!(out, "council findings still open (decide at approval):");
        for f in open {
            let _w = writeln!(out, "  {}", super::tasks::finding_line(f));
        }
    }
    if t.status == "awaiting_approval" {
        let _w = writeln!(
            out,
            "approve: openchef plan approve {0}   edit: openchef plan edit {0}   reject: openchef plan reject {0}",
            t.id
        );
    }
    out
}
