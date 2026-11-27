//! The evaluator (§9.2), ported from Go `internal/permissions/eval.go`: invariants,
//! then deny, ask and allow rules, then the mode's default. Deny always wins and no
//! mode overrides it.

use std::path::{Path, PathBuf};

use openchef_core::PermissionMode;
use openchef_runtime::INVARIANT_DENY_RULES;

use crate::rule::{is_edit, is_read, within};
use crate::shell::{Simple, split};
use crate::{Error, Policy, Rule, pathguard};

/// The evaluator's answer, from most to least permissive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Decision {
    /// Go ahead.
    Allow,
    /// A person decides.
    Ask,
    /// Refused.
    Deny,
}

impl Decision {
    /// `allow`, `ask` or `deny`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

impl std::fmt::Display for Decision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A decision and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The decision.
    pub decision: Decision,
    /// The rule that decided, if any (`invariant: …` for built-ins).
    pub rule: Option<String>,
    /// Shown to the agent on deny and to the user on ask.
    pub reason: String,
}

impl Verdict {
    fn new(decision: Decision, reason: impl Into<String>) -> Self {
        Self {
            decision,
            rule: None,
            reason: reason.into(),
        }
    }

    fn by(decision: Decision, rule: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            decision,
            rule: Some(rule.into()),
            reason: reason.into(),
        }
    }

    fn allow() -> Self {
        Self::new(Decision::Allow, "")
    }
}

/// One thing an agent wants to do, in Claude Code vocabulary.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// The tool (`Bash`, `Edit`, `Read`, `WebFetch`, `mcp__server__tool` …).
    pub tool: String,
    /// The shell command line, for `Bash`.
    pub command: Option<String>,
    /// The files concerned, for file tools.
    pub paths: Vec<PathBuf>,
    /// The address, for `WebFetch`.
    pub url: Option<String>,
}

/// Decides requests for one workspace.
#[derive(Debug, Clone)]
pub struct Evaluator {
    policy: Policy,
    workspace: PathBuf,
    home: PathBuf,
    /// Directories no agent may read: OpenChef's own state and secrets.
    private: Vec<PathBuf>,
    /// The built-in deny rules (history-rewriting git, protected directories).
    invariant: Vec<Rule>,
}

/// The worse of two verdicts; at equal decisions one backed by a rule wins.
fn worse(a: Verdict, b: Verdict) -> Verdict {
    if b.decision > a.decision || (b.decision == a.decision && a.rule.is_none() && b.rule.is_some())
    {
        b
    } else {
        a
    }
}

fn first(rules: &[Rule], matches: impl Fn(&Rule) -> bool) -> Option<&Rule> {
    rules.iter().find(|r| matches(r))
}

impl Evaluator {
    /// An evaluator for an agent working in `workspace`. `home` is the user's home,
    /// `private` lists directories agents may not read at all (e.g. `~/.openchef`).
    ///
    /// # Errors
    ///
    /// [`Error::Workspace`] for a relative workspace.
    pub fn new(
        policy: Policy,
        workspace: &Path,
        home: &Path,
        private: Vec<PathBuf>,
    ) -> Result<Self, Error> {
        if !workspace.is_absolute() {
            return Err(Error::Workspace(workspace.to_path_buf()));
        }
        let invariant = INVARIANT_DENY_RULES
            .iter()
            .map(|raw| Rule::parse(raw))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            policy,
            workspace: workspace.components().collect(),
            home: home.to_path_buf(),
            private,
            invariant,
        })
    }

    /// The effective permission mode.
    #[must_use]
    pub fn mode(&self) -> PermissionMode {
        self.policy.mode
    }

    /// Decides one request.
    #[must_use]
    pub fn evaluate(&self, req: &Request) -> Verdict {
        let v = if req.tool == "Bash" {
            self.bash(req.command.as_deref().unwrap_or(""))
        } else if is_edit(&req.tool) || is_read(&req.tool) {
            self.files(&req.tool, &req.paths)
        } else {
            self.other(req)
        };
        self.apply_mode(req, v)
    }

    /// Applies the mode to what the rules said (§9.1, §9.3):
    ///
    /// - deny is final in every mode;
    /// - plan is read-only: anything but reading is denied, even if a rule allows it;
    /// - a rule's allow or ask stands, except that dontAsk refuses what needs asking;
    /// - with no rule: reads inside the workspace are allowed; edits inside it are
    ///   allowed in acceptEdits, auto and bypassPermissions and ask in manual;
    ///   everything else asks, is denied in dontAsk, and is allowed in bypass.
    fn apply_mode(&self, req: &Request, v: Verdict) -> Verdict {
        let mode = self.policy.mode;
        let read = is_read(&req.tool);
        if v.decision == Decision::Deny {
            return v;
        }
        if mode == PermissionMode::Plan && !read {
            return Verdict::new(Decision::Deny, "plan mode is read-only");
        }
        let refuse = |v: Verdict| Verdict {
            decision: Decision::Deny,
            rule: v.rule,
            reason: format!("{} (dontAsk mode refuses what needs asking)", v.reason),
        };
        if v.rule.is_some() {
            if v.decision == Decision::Ask && mode == PermissionMode::DontAsk {
                return refuse(v);
            }
            return v;
        }
        if mode == PermissionMode::BypassPermissions {
            return Verdict::new(Decision::Allow, "bypassPermissions");
        }
        if v.decision == Decision::Allow {
            if read {
                return v;
            }
            if is_edit(&req.tool) {
                match mode {
                    PermissionMode::AcceptEdits | PermissionMode::Auto => return v,
                    PermissionMode::Manual => {
                        return Verdict::new(
                            Decision::Ask,
                            "file changes need approval in manual mode",
                        );
                    }
                    PermissionMode::Plan
                    | PermissionMode::DontAsk
                    | PermissionMode::BypassPermissions => {}
                }
            }
        }
        if mode == PermissionMode::DontAsk {
            return refuse(v);
        }
        Verdict::new(Decision::Ask, v.reason)
    }

    /// A command line: every simple command must be allowed; any denied one denies the
    /// whole line; an unparsable line asks.
    fn bash(&self, command: &str) -> Verdict {
        let cmds = match split(command) {
            Ok(c) => c,
            Err(e) => {
                return Verdict::new(
                    Decision::Ask,
                    format!("the command could not be parsed: {e}"),
                );
            }
        };
        if cmds.is_empty() {
            return Verdict::new(Decision::Ask, "the command runs nothing recognizable");
        }
        let mut worst = Verdict::allow();
        let mut allowed_by_rule = true;
        for s in &cmds {
            match self.simple(s) {
                Step::Deny(v) => return v,
                Step::Weigh { verdict, by_rule } => {
                    allowed_by_rule &= by_rule;
                    worst = worse(worst, verdict);
                }
            }
        }
        if worst.decision == Decision::Allow && allowed_by_rule {
            return Verdict::by(Decision::Allow, "allow rules", "every command is allowed");
        }
        worst
    }

    /// One simple command of a line.
    fn simple(&self, s: &Simple) -> Step {
        let matches = |r: &Rule| r.matches_tool("Bash") && r.matches_command(s);
        if let Some(r) = first(&self.invariant, matches) {
            return Step::Deny(Verdict::by(
                Decision::Deny,
                format!("invariant: {}", r.raw),
                format!(
                    "{:?} is reserved for OpenChef, which owns git history and refs",
                    s.line()
                ),
            ));
        }
        if let Some(r) = first(&self.policy.deny, matches) {
            return Step::Deny(Verdict::by(
                Decision::Deny,
                r.raw.clone(),
                format!("{:?} is denied by {}", s.line(), r.raw),
            ));
        }
        let mut worst = Verdict::allow();
        for w in &s.writes {
            let v = self.files("Write", &[self.abs(w)]);
            if v.decision == Decision::Deny {
                return Step::Deny(v);
            }
            worst = worse(worst, v);
        }
        for r in &s.reads {
            let v = self.files("Read", &[self.abs(r)]);
            if v.decision == Decision::Deny {
                return Step::Deny(v);
            }
        }
        if let Some(r) = first(&self.policy.ask, matches) {
            let v = Verdict::by(
                Decision::Ask,
                r.raw.clone(),
                format!("{:?} needs approval ({})", s.line(), r.raw),
            );
            return Step::Weigh {
                verdict: worse(worst, v),
                by_rule: true,
            };
        }
        if s.dynamic {
            let v = Verdict::new(
                Decision::Ask,
                format!("{:?} is only known at run time", s.line()),
            );
            return Step::Weigh {
                verdict: worse(worst, v),
                by_rule: false,
            };
        }
        if first(&self.policy.allow, matches).is_none() {
            let v = Verdict::new(Decision::Ask, format!("no rule allows {:?}", s.line()));
            return Step::Weigh {
                verdict: worse(worst, v),
                by_rule: false,
            };
        }
        Step::Weigh {
            verdict: worst,
            by_rule: true,
        }
    }

    /// A path a command names, made absolute.
    fn abs(&self, p: &str) -> PathBuf {
        if let Some(rest) = p.strip_prefix("~/") {
            return self.home.join(rest);
        }
        if cfg!(windows) && (p.starts_with('/') || p.starts_with('\\')) {
            // Rooted but without a drive (/etc/x in Git Bash, \x in cmd): never inside
            // the workspace. Resolve it on the workspace's volume so it counts as outside.
            let volume: PathBuf = self.workspace.components().take(2).collect();
            return volume.join(p.trim_start_matches(['/', '\\']));
        }
        let path = Path::new(p);
        if path.is_absolute() {
            path.components().collect()
        } else {
            self.workspace.join(path)
        }
    }

    fn private(&self, p: &Path) -> bool {
        self.private.iter().any(|dir| within(p, dir))
    }

    /// File tools on their paths: invariants, then rules; inside the workspace reads are
    /// allowed and edits follow the mode.
    fn files(&self, tool: &str, paths: &[PathBuf]) -> Verdict {
        if paths.is_empty() {
            return Verdict::new(Decision::Ask, "the request names no file");
        }
        let mut worst = Verdict::allow();
        for raw in paths {
            match self.file(tool, raw) {
                Err(v) => return v,
                Ok(v) => worst = worse(worst, v),
            }
        }
        worst
    }

    /// One path; `Err` is a deny that ends the request.
    fn file(&self, tool: &str, raw: &Path) -> Result<Verdict, Verdict> {
        let given = self.abs(&raw.to_string_lossy());
        let unchecked = |e: Error| {
            Verdict::by(
                Decision::Deny,
                "invariant: workspace",
                format!("OpenChef could not check {}: {e}", given.display()),
            )
        };
        // Rules and invariants see where the path really leads: a symlink in the
        // workspace must not turn an allowed path into a way out. Deny rules also see
        // the path as given, so either spelling of a denied file is refused.
        let resolved = pathguard::resolve(Some(&self.workspace), &given).map_err(unchecked)?;
        let inside = pathguard::is_within(&self.workspace, &resolved).map_err(unchecked)?;
        if self.private(&resolved) || self.private(&given) {
            return Err(Verdict::by(
                Decision::Deny,
                "invariant: private",
                format!("OpenChef's own files are off limits: {}", given.display()),
            ));
        }
        if is_edit(tool)
            && let Some(why) = pathguard::check(&self.workspace, &given).map_err(unchecked)?
        {
            return Err(Verdict::by(Decision::Deny, "invariant: workspace", why));
        }
        let matcher = |p: &Path| {
            let p = p.to_path_buf();
            move |r: &Rule| r.matches_tool(tool) && r.matches_path(&p, &self.workspace, &self.home)
        };
        // A path that leaves the workspace through a symlink inside it is judged where it
        // leads; other paths as the user's rules spell them (system links such as macOS
        // /etc -> /private/etc are not the agent's doing).
        let judged = if !within(&given, &self.workspace) || inside {
            given.clone()
        } else {
            resolved.clone()
        };
        let shown = given.display();
        for p in [&resolved, &given] {
            if let Some(r) = first(&self.policy.deny, matcher(p)) {
                return Err(Verdict::by(
                    Decision::Deny,
                    r.raw.clone(),
                    format!("{shown} is denied by {}", r.raw),
                ));
            }
        }
        if let Some(r) = first(&self.policy.ask, matcher(&judged)) {
            return Ok(Verdict::by(
                Decision::Ask,
                r.raw.clone(),
                format!("{shown} needs approval ({})", r.raw),
            ));
        }
        if let Some(r) = first(&self.policy.allow, matcher(&judged)) {
            return Ok(Verdict::by(Decision::Allow, r.raw.clone(), ""));
        }
        if inside {
            Ok(Verdict::allow())
        } else {
            Ok(Verdict::new(
                Decision::Ask,
                format!("{shown} is outside the workspace"),
            ))
        }
    }

    /// Non-file, non-shell tools (`WebFetch`, MCP …) by rules alone.
    fn other(&self, req: &Request) -> Verdict {
        let matches = |r: &Rule| {
            r.matches_tool(&req.tool)
                && if req.tool == "WebFetch" {
                    r.matches_url(req.url.as_deref().unwrap_or(""))
                } else {
                    r.specifier.is_empty()
                }
        };
        let tool = &req.tool;
        if let Some(r) = first(&self.policy.deny, matches) {
            return Verdict::by(
                Decision::Deny,
                r.raw.clone(),
                format!("{tool} is denied by {}", r.raw),
            );
        }
        if let Some(r) = first(&self.policy.ask, matches) {
            return Verdict::by(
                Decision::Ask,
                r.raw.clone(),
                format!("{tool} needs approval ({})", r.raw),
            );
        }
        if let Some(r) = first(&self.policy.allow, matches) {
            return Verdict::by(Decision::Allow, r.raw.clone(), "");
        }
        Verdict::new(Decision::Ask, format!("no rule allows {tool}"))
    }
}

/// The outcome of one simple command.
enum Step {
    /// Denies the whole line.
    Deny(Verdict),
    /// Joins the line's verdict; `by_rule` is false when no allow rule covered it.
    Weigh { verdict: Verdict, by_rule: bool },
}
