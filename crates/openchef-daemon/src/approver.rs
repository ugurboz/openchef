//! The interim permission policy until the evaluator and approval queue of R4 (§9)
//! exist, ported from Go `internal/app/approver.go` and made aware of the task's
//! permission mode. With nobody to ask yet:
//!
//! - file changes inside the task's worktree are allowed, except under the protected
//!   directories (`.git`, `.openchef`, `.claude`) and except in `plan` mode;
//! - shell commands and other tools are allowed only in `bypassPermissions`, which a
//!   user asks for per task and which only ever runs in an isolated worktree;
//! - everything else is refused with an explanation. OpenChef runs the step's
//!   verification itself, so a step still completes without the agent running commands.

use std::path::PathBuf;

use openchef_core::PermissionMode;
use openchef_permissions::pathguard;
use openchef_runtime::{
    ApprovalRequest, Approver, ApproverError, BoxFuture, Decision, Question, RequestKind,
};
use tokio_util::sync::CancellationToken;

const COMMANDS_DENIED: &str = "OpenChef does not let agents run shell commands or other tools \
    in this permission mode yet (approvals arrive in R4). Edit files only; OpenChef runs the \
    step's verification command itself.";

const PLAN_DENIED: &str =
    "This task runs in plan mode, which is read-only: no files may be changed.";

const MODE_CHANGE_DENIED: &str = "OpenChef does not change a task's permission mode during a \
    session; keep working in the current mode.";

const NO_ANSWER: &str = "No one is available to answer right now. Make the most reasonable \
    choice, say which one you made in your final answer, and continue.";

/// Decides for one session of a task.
#[derive(Debug, Clone)]
pub(crate) struct WorkspaceApprover {
    /// The task's worktree.
    pub(crate) dir: PathBuf,
    /// The task's permission mode.
    pub(crate) mode: PermissionMode,
}

impl WorkspaceApprover {
    pub(crate) fn decide(&self, req: &ApprovalRequest) -> Decision {
        match req.kind {
            RequestKind::FileChange => self.file_change(&req.paths),
            RequestKind::Command | RequestKind::Tool => match self.mode {
                PermissionMode::BypassPermissions => Decision::allow(),
                PermissionMode::Manual
                | PermissionMode::AcceptEdits
                | PermissionMode::Plan
                | PermissionMode::Auto
                | PermissionMode::DontAsk => Decision::deny(COMMANDS_DENIED),
            },
            RequestKind::ModeChange => Decision::deny(MODE_CHANGE_DENIED),
        }
    }

    fn file_change(&self, paths: &[String]) -> Decision {
        if self.mode == PermissionMode::Plan {
            return Decision::deny(PLAN_DENIED);
        }
        if paths.is_empty() {
            return Decision::deny(
                "OpenChef could not tell which files this change touches, so it refused it.",
            );
        }
        for p in paths {
            match pathguard::check(&self.dir, std::path::Path::new(p)) {
                Ok(None) => {}
                Ok(Some(why)) => return Decision::deny(capitalize(&why) + "."),
                Err(e) => {
                    return Decision::deny(format!(
                        "OpenChef could not check this path, so it refused the change: {e}"
                    ));
                }
            }
        }
        Decision::allow()
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars
        .next()
        .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
}

impl Approver for WorkspaceApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<Decision, ApproverError>> {
        let decision = self.decide(&request);
        Box::pin(async move { Ok(decision) })
    }

    fn answer(
        &self,
        _question: Question,
        _cancel: CancellationToken,
    ) -> BoxFuture<'static, Result<String, ApproverError>> {
        Box::pin(async { Ok(NO_ANSWER.to_owned()) })
    }
}

#[cfg(test)]
mod tests {
    use openchef_core::PermissionMode;
    use openchef_runtime::{ApprovalRequest, RequestKind, SessionRef};
    use serde_json::Value;

    use super::WorkspaceApprover;

    fn req(kind: RequestKind, paths: &[&str]) -> ApprovalRequest {
        ApprovalRequest {
            session: SessionRef {
                runtime: "fake".into(),
                id: "s".into(),
            },
            kind,
            tool: "Edit".into(),
            command: None,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
            description: String::new(),
            input: Value::Null,
            raw: Value::Null,
        }
    }

    #[test]
    fn workspace_approver() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join(".git")).unwrap_or_else(|e| panic!("{e}"));
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.path(), ws.join("escape")).unwrap_or_else(|e| panic!("{e}"));
        let p = |rel: &str| ws.join(rel).to_string_lossy().into_owned();
        let outside = dir
            .path()
            .join("outside.txt")
            .to_string_lossy()
            .into_owned();
        let manual = WorkspaceApprover {
            dir: ws.clone(),
            mode: PermissionMode::Manual,
        };
        let edit = |paths: &[&str]| req(RequestKind::FileChange, paths);

        for r in [edit(&[&p("main.go")]), edit(&["rel/new.go"])] {
            let d = manual.decide(&r);
            assert!(d.allow, "{r:?} denied: {d:?}");
        }
        let mut denied = vec![
            req(RequestKind::Command, &[]),
            req(RequestKind::Tool, &[]),
            req(RequestKind::ModeChange, &[]),
            edit(&[]),
            edit(&[&outside]),
            edit(&[&p(".git/hooks/pre-commit")]),
            edit(&[&p("main.go"), &p(".claude/settings.json")]),
        ];
        if cfg!(unix) {
            denied.push(edit(&[&p("escape/outside.txt")]));
        }
        if cfg!(any(target_os = "macos", windows)) {
            denied.push(edit(&[&p(".GIT/hooks/post-commit")]));
        }
        for r in &denied {
            let d = manual.decide(r);
            assert!(!d.allow && !d.message.is_empty(), "{r:?} allowed: {d:?}");
        }
        let none = WorkspaceApprover {
            dir: std::path::PathBuf::new(),
            mode: PermissionMode::Manual,
        };
        assert!(
            !none.decide(&edit(&["x.go"])).allow,
            "no workspace allowed a relative write"
        );
        let why = manual.decide(&edit(&[&p(".git/hooks/x")])).message;
        assert!(
            why.starts_with("Files under .git are managed by OpenChef"),
            "{why}"
        );
    }

    #[test]
    fn modes() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        let inside = dir.path().join("a.txt").to_string_lossy().into_owned();
        let hook = dir.path().join(".git/x").to_string_lossy().into_owned();
        let a = |mode| WorkspaceApprover {
            dir: dir.path().to_path_buf(),
            mode,
        };
        assert!(
            !a(PermissionMode::Plan)
                .decide(&req(RequestKind::FileChange, &[&inside]))
                .allow
        );
        for mode in [
            PermissionMode::AcceptEdits,
            PermissionMode::Auto,
            PermissionMode::DontAsk,
        ] {
            assert!(
                a(mode)
                    .decide(&req(RequestKind::FileChange, &[&inside]))
                    .allow
            );
            assert!(!a(mode).decide(&req(RequestKind::Command, &[])).allow);
        }
        let bypass = a(PermissionMode::BypassPermissions);
        assert!(bypass.decide(&req(RequestKind::Command, &[])).allow);
        assert!(
            !bypass.decide(&req(RequestKind::FileChange, &[&hook])).allow,
            "protected directories hold in bypass too"
        );
    }
}
