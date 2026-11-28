//! Messages to agents. Every message restates the step in full (§7.2), so a resumed or
//! fresh session never depends on what an earlier turn said.

use std::fmt::Write as _;

use openchef_core::{Kind, Step, Task};

use crate::verify::Outcome;

fn restate(t: &Task, s: &Step, total: usize) -> String {
    let mut b = format!(
        "Step {} of {total}: {}\n\n{}\n",
        s.index,
        s.title,
        s.prompt.trim()
    );
    // Writing to a String cannot fail.
    if !s.files.is_empty() {
        let _w = write!(b, "\nScope: work only in {}.\n", s.files.join(", "));
    }
    if let Some(v) = &s.verify {
        let _w = write!(
            b,
            "\nWhen you finish, this command must pass in the workspace: {v}\n"
        );
    }
    b.push_str(
        "\nOpenChef commits your changes after verification. Do not run git commands that create commits or \
         move refs (commit, reset, rebase, checkout, switch, merge, push, tag).\n",
    );
    if t.kind == Kind::Pipeline {
        let _w = write!(b, "\nOverall request: {}\n", t.prompt.trim());
    }
    b
}

pub(crate) fn step_prompt(t: &Task, s: &Step, total: usize) -> String {
    restate(t, s, total)
}

pub(crate) fn retry_prompt(t: &Task, s: &Step, total: usize, res: &Outcome) -> String {
    format!(
        "Verification {}. Fix the problem and finish the step.\n\n{}\nOutput of {}:\n```\n{}\n```\n",
        res.summary(),
        restate(t, s, total),
        s.verify.as_deref().unwrap_or(""),
        res.output.trim_end_matches('\n')
    )
}

pub(crate) fn loop_prompt(t: &Task, s: &Step, total: usize, repeats: u32) -> String {
    format!(
        "You repeated the same tool call {repeats} times in a row, so OpenChef stopped the turn. Step back, find \
         out why it is not working, and take a different approach.\n\n{}",
        restate(t, s, total)
    )
}

pub(crate) fn stall_prompt(t: &Task, s: &Step, total: usize, why: &str) -> String {
    format!(
        "OpenChef stopped the last turn: {why}. Continue from the current state of the workspace and finish the \
         step.\n\n{}",
        restate(t, s, total)
    )
}

pub(crate) fn resume_prompt(t: &Task, s: &Step, total: usize) -> String {
    format!(
        "Your previous process ended unexpectedly. Check the current state of the workspace and continue.\n\n{}",
        restate(t, s, total)
    )
}

/// §5.3's note followed by the message the restart interrupted.
pub(crate) fn recovery_prompt(s: &Step, interrupted: &str) -> String {
    format!(
        "The orchestrator restarted during your last turn. Check the workspace state and continue Step {}.\n\n{interrupted}",
        s.index
    )
}

pub(crate) fn handoff_prompt(
    t: &Task,
    s: &Step,
    total: usize,
    from: &str,
    last_commit: &str,
    patch: Option<&str>,
) -> String {
    let mut b = format!(
        "You are taking over this step from another agent ({from}) that ran out of quota. The workspace has been \
         reset to the last verified commit {}.",
        short(last_commit)
    );
    if let Some(p) = patch {
        let _w = write!(
            b,
            " Its unfinished work is saved in {p}; read it, and apply it with `git apply --binary {p}` if it helps."
        );
    }
    b.push_str("\n\n");
    b.push_str(&restate(t, s, total));
    b
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The escalation turn (§7.5, D-040): another agent's loop on this step stopped making
/// progress; one turn on the worktree as it was left.
pub(crate) fn escalation_prompt(
    t: &Task,
    s: &Step,
    total: usize,
    from: &str,
    why: &str,
    evidence: &str,
) -> String {
    let mut b = format!(
        "You are taking over this step from another agent ({from}) whose work stopped making progress: {why}. \
         The workspace holds its changes as it left them. Find the real cause and finish the step in this one \
         turn; verification runs once after your answer.\n"
    );
    if !evidence.trim().is_empty() {
        let _w = write!(
            b,
            "\nWhat kept failing:\n```\n{}\n```\n",
            evidence.trim_end()
        );
    }
    b.push_str(
        "\nIf it concerns reviewer findings, end your answer with one line per finding you fixed: `FIXED <id>`.\n\n",
    );
    b.push_str(&restate(t, s, total));
    b
}
