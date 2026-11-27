//! Permission rules in Claude Code syntax (§9.1), ported from Go
//! `internal/permissions/rule.go`: `Bash(npm run test:*)`, `Edit(src/**)`,
//! `WebFetch(domain:github.com)`, `mcp__github__create_issue`.

use std::path::{Component, Path};

use crate::Error;
use crate::shell::Simple;

/// Tools that change files: `Edit` rules cover all of them, as in Claude Code.
pub(crate) const EDIT_TOOLS: &[&str] = &["Edit", "Write", "MultiEdit", "NotebookEdit"];
/// Tools that read files: `Read` rules cover all of them.
pub(crate) const READ_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS", "NotebookRead"];

pub(crate) fn is_edit(tool: &str) -> bool {
    EDIT_TOOLS.contains(&tool)
}

pub(crate) fn is_read(tool: &str) -> bool {
    READ_TOOLS.contains(&tool)
}

/// One parsed permission rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// The rule as written (trimmed).
    pub raw: String,
    /// The tool name, or an `mcp__server[__tool]` prefix.
    pub tool: String,
    /// The text inside the parentheses; empty matches any use of the tool.
    pub specifier: String,
}

fn bad(raw: &str, why: impl Into<String>) -> Error {
    Error::BadRule {
        rule: raw.to_owned(),
        reason: why.into(),
    }
}

impl Rule {
    /// Parses a rule in Claude Code syntax.
    ///
    /// # Errors
    ///
    /// [`Error::BadRule`] for a malformed rule.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(bad(raw, "empty"));
        }
        let (name, spec) = match s.find('(') {
            None => (s, ""),
            Some(i) => {
                let Some(inner) = s[i + 1..].strip_suffix(')') else {
                    return Err(bad(raw, "missing closing parenthesis"));
                };
                if inner.trim().is_empty() {
                    let name = &s[..i];
                    return Err(bad(
                        raw,
                        format!("empty specifier (write {name} alone to match any use)"),
                    ));
                }
                (&s[..i], inner)
            }
        };
        if name.is_empty() || name.contains([' ', '(', ')']) {
            return Err(bad(raw, "bad tool name"));
        }
        if name.starts_with("mcp__") && !spec.is_empty() {
            return Err(bad(raw, "MCP rules take no specifier"));
        }
        if name == "Bash" && spec.strip_suffix(":*").is_some_and(str::is_empty) {
            return Err(bad(raw, "empty command prefix"));
        }
        if name == "WebFetch" && !spec.is_empty() && !spec.starts_with("domain:") {
            return Err(bad(raw, "WebFetch rules take domain:<host>"));
        }
        Ok(Self {
            raw: s.to_owned(),
            tool: name.to_owned(),
            specifier: spec.to_owned(),
        })
    }

    /// Whether the rule applies to `tool` at all.
    pub(crate) fn matches_tool(&self, tool: &str) -> bool {
        if self.tool.starts_with("mcp__") {
            if !tool.starts_with("mcp__") {
                return false;
            }
            let prefix = self.tool.trim_end_matches('*');
            let prefix = prefix.strip_suffix("__").unwrap_or(prefix);
            return tool == prefix || tool.starts_with(&format!("{prefix}__"));
        }
        match self.tool.as_str() {
            "Edit" => is_edit(tool),
            "Read" => is_read(tool),
            other => other == tool,
        }
    }

    /// Whether a Bash rule matches one simple command.
    ///
    /// - `Bash(npm run test:*)` matches commands whose words start with `npm run test`.
    /// - `Bash(npm run *)` is a wildcard over the whole line (`*` spans spaces).
    /// - `Bash(npm test)` matches exactly.
    ///
    /// A dynamic command matches only through its static leading words, and only with a
    /// prefix rule; deny rules use that to catch e.g. `rm -rf $DIR` with `Bash(rm:*)`.
    pub(crate) fn matches_command(&self, s: &Simple) -> bool {
        if self.specifier.is_empty() {
            return true;
        }
        if let Some(prefix) = self.specifier.strip_suffix(":*") {
            let want: Vec<&str> = prefix.split_whitespace().collect();
            return want.len() <= s.args.len() && want.iter().zip(&s.args).all(|(w, a)| w == a);
        }
        if s.dynamic {
            return false;
        }
        if self.specifier.contains('*') {
            return wildcard(&self.specifier, &s.line());
        }
        self.specifier == s.line()
    }

    /// Whether a file rule matches the absolute path `p`. Patterns follow Claude Code:
    /// `//abs` is absolute, `~/x` is under home, `/x` is relative to the project root and
    /// `x` or `./x` to the working directory; OpenChef agents work in a worktree with
    /// the repository's layout, so both resolve against the workspace. `**` matches any
    /// number of directories, `*` and `?` stay within one name.
    pub(crate) fn matches_path(&self, p: &Path, workspace: &Path, home: &Path) -> bool {
        if self.specifier.is_empty() {
            return true;
        }
        let ws = slash(workspace);
        let spec = self.specifier.as_str();
        let relative = spec.strip_prefix("./").unwrap_or(spec);
        let pattern = if let Some(abs) = spec.strip_prefix("//") {
            if abs.len() >= 2 && abs.as_bytes()[1] == b':' {
                abs.to_owned() // //C:/x on Windows
            } else {
                format!("/{abs}")
            }
        } else if let Some(rest) = spec.strip_prefix("~/") {
            format!("{}/{rest}", slash(home))
        } else if spec.starts_with('/') {
            format!("{ws}{spec}")
        } else if !relative.contains('/') {
            // A pattern without a slash (e.g. "*.env") matches at any depth, as in
            // gitignore.
            format!("{ws}/**/{relative}")
        } else {
            format!("{ws}/{relative}")
        };
        glob_match(&clean(&pattern), &clean(&slash(p)))
    }

    /// Whether a `WebFetch` rule matches `raw`: `domain:example.com` matches that host,
    /// `domain:*.example.com` its subdomains.
    pub(crate) fn matches_url(&self, raw: &str) -> bool {
        if self.specifier.is_empty() {
            return true;
        }
        let Some(host) = host_of(raw) else {
            return false;
        };
        let want = self
            .specifier
            .strip_prefix("domain:")
            .unwrap_or(&self.specifier)
            .to_lowercase();
        match want.strip_prefix("*.") {
            Some(sub) => host.ends_with(&format!(".{sub}")),
            None => host == want,
        }
    }
}

/// The lower-cased host of an absolute URL, without user info or port.
fn host_of(raw: &str) -> Option<String> {
    let (_, rest) = raw.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    let host = if let Some(v6) = host_port.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        host_port.split(':').next()?
    };
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// A path with `/` separators.
fn slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Lexically cleans a slash path (like Go's `path.Clean`).
fn clean(p: &str) -> String {
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    let _popped = out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    if rooted { format!("/{joined}") } else { joined }
}

/// Matches `s` against `pattern` where `*` stands for any run of characters.
fn wildcard(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let Some((first, rest)) = parts.split_first() else {
        return pattern == s;
    };
    let Some(mut s) = s.strip_prefix(first) else {
        return false;
    };
    let Some((last, middle)) = rest.split_last() else {
        return s.is_empty();
    };
    for part in middle {
        match s.find(part) {
            Some(j) => s = &s[j + part.len()..],
            None => return false,
        }
    }
    s.ends_with(last)
}

/// Matches slash-separated paths with `*`, `?` and `**` (any directories).
fn glob_match(pattern: &str, name: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').collect();
    let name: Vec<&str> = name.split('/').collect();
    match_segments(&pat, &name)
}

fn match_segments(pat: &[&str], name: &[&str]) -> bool {
    let Some((first, rest)) = pat.split_first() else {
        // A directory pattern also covers what is inside it (Read(./secrets) covers
        // ./secrets/key).
        return true;
    };
    if *first == "**" {
        if rest.is_empty() {
            return true; // ** at the end matches everything below
        }
        return (0..=name.len()).any(|i| match_segments(rest, &name[i..]));
    }
    match name.split_first() {
        Some((n, name_rest)) => segment_match(first, n) && match_segments(rest, name_rest),
        None => false,
    }
}

/// One path element against a pattern with `*` (any run) and `?` (one character).
fn segment_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    let (mut pi, mut ni) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whether `p` is `dir` or below it, lexically.
pub(crate) fn within(p: &Path, dir: &Path) -> bool {
    fn normal(x: &Path) -> Vec<Component<'_>> {
        x.components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect()
    }
    let (p, dir) = (normal(p), normal(dir));
    p.len() >= dir.len() && p.iter().zip(&dir).all(|(a, b)| a == b)
}

#[cfg(test)]
mod tests {
    use super::{clean, glob_match, host_of, segment_match, wildcard};

    #[test]
    fn helpers() {
        assert!(wildcard("npm run *", "npm run build"));
        assert!(!wildcard("npm run *", "npm install"));
        assert!(wildcard("* --version", "node --version"));
        assert!(wildcard("a*b*c", "aXbYc"));
        assert!(segment_match("*.pem", "key.pem"));
        assert!(segment_match("a?c", "abc"));
        assert!(!segment_match("*.pem", "key.pem.bak"));
        assert!(glob_match("/w/**/*.pem", "/w/certs/deep/key.pem"));
        assert!(glob_match("/w/secrets", "/w/secrets/key"));
        assert_eq!(clean("/w/repo/../other/./x"), "/w/other/x");
        assert_eq!(clean("/../x"), "/x");
        assert_eq!(
            host_of("https://u@API.github.com:443/x").as_deref(),
            Some("api.github.com")
        );
        assert_eq!(host_of("not a url"), None);
    }
}
