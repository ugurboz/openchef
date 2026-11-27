//! Prints engine events for people (D-014): streamed text as it arrives, one line per
//! tool call, verification and step outcomes. `run` and `logs -f` share it, so both
//! always show the same thing. With `json` it prints one event per line instead.

use std::io::{self, Write};

use openchef_core::{Approval, ApprovalKind, ApprovalStatus, AttemptStatus, DecidedBy, StepStatus};
use openchef_engine::{Event, EventKind};
use openchef_runtime::{EventBody, ToolCall};
use time::OffsetDateTime;

/// Renders a stream of events to a writer.
pub(crate) struct Renderer<'w> {
    w: &'w mut dyn Write,
    json: bool,
    quiet: bool,
    /// Agent text is being streamed without a trailing newline.
    mid_text: bool,
}

impl<'w> Renderer<'w> {
    pub(crate) fn new(w: &'w mut dyn Write, json: bool, quiet: bool) -> Self {
        Self {
            w,
            json,
            quiet,
            mid_text: false,
        }
    }

    /// Writes one line, ending streamed text first.
    pub(crate) fn line(&mut self, text: &str) -> io::Result<()> {
        if self.mid_text {
            writeln!(self.w)?;
            self.mid_text = false;
        }
        writeln!(self.w, "{text}")
    }

    /// Renders one event.
    pub(crate) fn event(&mut self, ev: &Event) -> io::Result<()> {
        if self.json {
            let line = serde_json::to_string(ev).map_err(io::Error::other)?;
            return writeln!(self.w, "{line}");
        }
        if self.quiet {
            // Quiet still shows what needs the user: the outcome.
            return if ev.kind == EventKind::TaskStatus {
                self.status(ev)
            } else {
                Ok(())
            };
        }
        let msg = ev.message.as_deref().unwrap_or("");
        let step = ev.step.unwrap_or(0);
        match ev.kind {
            EventKind::TaskStatus => self.status(ev),
            EventKind::StepStarted => self.line(&format!("▸ Step {step}: {msg}")),
            EventKind::AttemptStarted => {
                self.line(&format!("  agent {}", ev.agent.as_deref().unwrap_or("")))
            }
            EventKind::Agent => match &ev.runtime {
                Some(r) => self.agent(&r.body),
                None => Ok(()),
            },
            EventKind::VerifyStarted => self.line(&format!("  ▶ verify  {msg}")),
            EventKind::VerifyFinished => match &ev.verify {
                Some(v) => self.line(&format!(
                    "  {} verification {}",
                    if v.passed { "✓" } else { "✗" },
                    v.summary()
                )),
                None => Ok(()),
            },
            EventKind::AttemptEnded => {
                if ev.status.as_deref() == Some(AttemptStatus::Succeeded.as_str()) {
                    Ok(())
                } else {
                    self.line(&format!(
                        "  agent {} stopped: {msg}",
                        ev.agent.as_deref().unwrap_or("")
                    ))
                }
            }
            EventKind::StepFinished => {
                if ev.status.as_deref() == Some(StepStatus::Verified.as_str()) {
                    self.line(&format!("  ✓ step {step} done ({})", short_sha(msg)))
                } else {
                    self.line(&format!("  ✗ step {step} failed: {msg}"))
                }
            }
            EventKind::Notice => self.line(&format!("  · {msg}")),
            EventKind::ApprovalRequested => self.approval(ev, true),
            EventKind::ApprovalDecided => self.approval(ev, false),
        }
    }

    /// A request (with the commands that settle it) or its decision.
    fn approval(&mut self, ev: &Event, requested: bool) -> io::Result<()> {
        let Some(a) = &ev.approval else {
            return Ok(());
        };
        if requested {
            // Every line of a multi-line command stays visible: a person approves
            // exactly what runs.
            let what = sanitize(&what(a)).trim_end().replace('\n', "\n      ");
            self.line(&format!("  ? #{} {} {what}", a.id, a.agent))?;
            return if a.kind == ApprovalKind::Question {
                self.line(&format!("    answer: openchef answer {} \"…\"", a.id))
            } else {
                self.line(&format!(
                    "    allow: openchef approve {0}   deny: openchef deny {0}",
                    a.id
                ))
            };
        }
        let mut line = format!("  · #{} {}", a.id, a.status);
        if let Some(by) = a.decided_by.filter(|b| *b != DecidedBy::User) {
            line = format!("{line} ({by})");
        }
        if a.kind == ApprovalKind::Question && a.status == ApprovalStatus::Answered {
            line = format!(
                "{line}: {}",
                first_line(a.answer.as_deref().unwrap_or(""), 80)
            );
        }
        self.line(&line)
    }

    fn status(&mut self, ev: &Event) -> io::Result<()> {
        let msg = ev.message.as_deref().unwrap_or("");
        match ev.status.as_deref().unwrap_or("") {
            "running" if !msg.is_empty() && msg != "created" => {
                self.line(&format!("◆ running ({msg})"))
            }
            "running" => Ok(()),
            "done" => self.line("✔ done"),
            "waiting_quota" => self.line(&format!("⏸ waiting for quota: {msg}")),
            "needs_attention" => self.line(&format!("! needs attention: {msg}")),
            "failed" => self.line(&format!("✖ failed: {msg}")),
            "cancelled" => self.line("✖ cancelled"),
            other => self.line(&format!("◆ {other}")),
        }
    }

    fn agent(&mut self, body: &EventBody) -> io::Result<()> {
        match body {
            EventBody::Text(t) if !t.is_empty() => {
                let mut text = sanitize(t);
                if !self.mid_text {
                    text.insert_str(0, "  ");
                }
                let text = text.replace('\n', "\n  ");
                self.w.write_all(text.as_bytes())?;
                self.mid_text = !t.ends_with('\n');
                Ok(())
            }
            EventBody::ToolCall(tc) => self.line(&format!("  ● {}  {}", tc.name, tool_summary(tc))),
            EventBody::ToolResult(r) if r.is_error => {
                self.line(&format!("    ⎿ error: {}", first_line(&r.output, 120)))
            }
            EventBody::Quota(q) if q.exhausted => self.line("  · quota exhausted"),
            EventBody::TurnFailed(f) => self.line(&format!(
                "  · turn ended: {} {}",
                f.kind.as_str(),
                first_line(&f.message, 120)
            )),
            EventBody::Text(_)
            | EventBody::ToolResult(_)
            | EventBody::Quota(_)
            | EventBody::TurnCompleted(_) => Ok(()),
        }
    }

    /// Ends streamed text so the shell prompt starts on its own line.
    pub(crate) fn finish(&mut self) -> io::Result<()> {
        if self.mid_text {
            writeln!(self.w)?;
            self.mid_text = false;
        }
        Ok(())
    }
}

/// A request as one reads it: the tool and its argument, or the question.
pub(crate) fn what(a: &Approval) -> String {
    if a.kind == ApprovalKind::Question {
        let mut s = format!("asks: {}", a.summary);
        if !a.options.is_empty() {
            s = format!("{s} ({})", a.options.join(", "));
        }
        return s;
    }
    match &a.tool {
        Some(tool) => format!("{tool}: {}", a.summary),
        None => a.summary.clone(),
    }
}

/// Drops control characters other than newline and tab from agent text, so output
/// prompted by repository content cannot drive the terminal (ANSI/OSC escapes).
pub(crate) fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            c == '\n'
                || c == '\t'
                || !(c < '\u{20}' || c == '\u{7f}' || ('\u{80}'..'\u{a0}').contains(&c))
        })
        .collect()
}

/// The most telling argument of a tool call: the command for Bash, the path for file
/// tools.
fn tool_summary(tc: &ToolCall) -> String {
    ["command", "file_path", "path", "pattern", "url", "query"]
        .iter()
        .find_map(|k| tc.input[*k].as_str().filter(|v| !v.is_empty()))
        .map_or_else(String::new, |v| first_line(v, 100))
}

/// The first line, sanitized and cut to `limit` characters.
pub(crate) fn first_line(s: &str, limit: usize) -> String {
    let clean = sanitize(s.trim());
    let line = clean.lines().next().unwrap_or("");
    if line.chars().count() > limit {
        let cut: String = line.chars().take(limit - 1).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    }
}

/// Shortens the commit at the start of `s` and keeps any note after it.
pub(crate) fn short_sha(s: &str) -> String {
    let (sha, rest) = s.split_once(' ').map_or((s, None), |(a, b)| (a, Some(b)));
    let sha = sha.get(..12).unwrap_or(sha);
    rest.map_or_else(|| sha.to_owned(), |r| format!("{sha} {r}"))
}

/// An age for tables.
pub(crate) fn since(t: OffsetDateTime, now: OffsetDateTime) -> String {
    let d = now - t;
    if d < time::Duration::minutes(1) {
        "just now".to_owned()
    } else if d < time::Duration::hours(1) {
        format!("{}m ago", d.whole_minutes())
    } else if d < time::Duration::hours(48) {
        format!("{}h ago", d.whole_hours())
    } else {
        format!("{}d ago", d.whole_days())
    }
}

#[cfg(test)]
mod tests {
    use openchef_core::TaskId;
    use openchef_engine::{Event, EventKind};
    use openchef_runtime::{EventBody, ToolCall};
    use serde_json::json;
    use time::OffsetDateTime;

    use super::{Renderer, first_line, sanitize, short_sha, since};

    fn ev(kind: EventKind) -> Event {
        Event::new(
            &"k3m9x2q7ab"
                .parse::<TaskId>()
                .unwrap_or_else(|_| unreachable!()),
            OffsetDateTime::UNIX_EPOCH,
            kind,
        )
    }

    fn agent(body: EventBody) -> Event {
        let mut e = ev(EventKind::Agent);
        e.runtime = Some(openchef_runtime::Event {
            at: OffsetDateTime::UNIX_EPOCH,
            body,
        });
        e
    }

    fn render(events: &[Event], quiet: bool) -> String {
        let mut out = Vec::new();
        let mut r = Renderer::new(&mut out, false, quiet);
        for e in events {
            r.event(e).unwrap_or(());
        }
        r.finish().unwrap_or(());
        String::from_utf8(out).unwrap_or_default()
    }

    #[test]
    fn renders_a_run() {
        let mut step = ev(EventKind::StepStarted);
        step.step = Some(1);
        step.message = Some("add out.txt".into());
        let mut finished = ev(EventKind::StepFinished);
        finished.step = Some(1);
        finished.status = Some("verified".into());
        finished.message = Some("0123456789abcdef0123".into());
        let mut done = ev(EventKind::TaskStatus);
        done.status = Some("done".into());
        let call = ToolCall {
            id: "c".into(),
            name: "Bash".into(),
            input: json!({"command": "go test ./..."}),
            signature: String::new(),
        };
        let events = [
            step,
            agent(EventBody::Text("Adding".into())),
            agent(EventBody::Text(" the file.\x1b]52;c;evil\x07".into())),
            agent(EventBody::ToolCall(call)),
            finished,
            done,
        ];
        let out = render(&events, false);
        assert_eq!(
            out,
            "▸ Step 1: add out.txt\n  Adding the file.]52;c;evil\n  ● Bash  go test ./...\n  ✓ step 1 done (0123456789ab)\n✔ done\n"
        );
        assert_eq!(
            render(&events, true),
            "✔ done\n",
            "quiet shows only the outcome"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(sanitize("a\x1b[31mb\tc\n"), "a[31mb\tc\n");
        assert_eq!(first_line("  one\ntwo", 10), "one");
        assert_eq!(first_line("abcdefghijkl", 5), "abcd…");
        assert_eq!(
            short_sha("0123456789abcdef (found after a restart)"),
            "0123456789ab (found after a restart)"
        );
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
        assert_eq!(since(now - time::Duration::seconds(5), now), "just now");
        assert_eq!(since(now - time::Duration::minutes(5), now), "5m ago");
        assert_eq!(since(now - time::Duration::hours(3), now), "3h ago");
        assert_eq!(since(now - time::Duration::days(3), now), "3d ago");
    }
}
