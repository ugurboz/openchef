//! Splits a shell command line into its simple commands (§9.2), so permission rules
//! match commands, never strings. Ported from Go `internal/permissions/shell.go` on top
//! of `brush-parser` (spike S4, D-031).
//!
//! Every simple command of a line is reported; words that expand at run time make a
//! command dynamic; opaque runners (eval, xargs, sudo …) are dynamic; wrappers (env,
//! timeout, nice …) are unwrapped; `sh -c` strings are split recursively; redirection
//! targets are file writes and reads. Anything not modelled fails closed: the line is
//! unparsable, which the evaluator turns into "ask".

use std::io::BufReader;

use brush_parser::ast::{
    AndOr, Command, CommandPrefixOrSuffixItem, CompoundCommand, CompoundList, ExtendedTestExpr,
    IoFileRedirectKind, IoFileRedirectTarget, IoRedirect, Pipeline, SimpleCommand,
};
use brush_parser::word::{WordPiece, WordPieceWithSource};
use brush_parser::{Parser, ParserOptions};

use crate::Error;

/// One simple command of a command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Simple {
    /// The words, when they are known before run time.
    pub args: Vec<String>,
    /// Some word or the environment is only known at run time (an expansion, a glob,
    /// an opaque runner such as `eval` or `sudo`).
    pub dynamic: bool,
    /// Files the command's redirections write.
    pub writes: Vec<String>,
    /// Files the command's redirections read.
    pub reads: Vec<String>,
}

impl Simple {
    /// The command as one line, words separated by spaces.
    #[must_use]
    pub fn line(&self) -> String {
        self.args.join(" ")
    }
}

/// The parser's stack (D-031): enough for [`MAX_NESTING`] levels in debug builds.
const PARSER_STACK: usize = 8 * 1024 * 1024;

fn unparsable(reason: impl Into<String>) -> Error {
    Error::Unparsable(reason.into())
}

/// Most nested `sh -c` levels followed.
const MAX_DEPTH: usize = 4;

/// Longest command line parsed; longer ones are unparsable (ask).
pub const MAX_LEN: usize = 64 * 1024;

/// Most nested openings (`(`, `{`, `$(`, backquote) allowed. brush-parser recurses on
/// the native stack and a deeply nested line overflows it, which aborts the process
/// (spike S4): the guard keeps a hostile line from crashing the daemon.
pub const MAX_NESTING: usize = 64;

/// Conservative nesting estimate: the running count of openings minus closings,
/// ignoring quotes (over-counting only makes the guard stricter).
fn too_nested(command: &str) -> bool {
    let mut depth: usize = 0;
    let mut deepest: usize = 0;
    let mut backquotes = 0_usize;
    for c in command.chars() {
        match c {
            '(' | '{' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            ')' | '}' => depth = depth.saturating_sub(1),
            '`' => backquotes += 1,
            _ => {}
        }
    }
    deepest > MAX_NESTING || backquotes > MAX_NESTING
}

fn is_opaque(name: &str) -> bool {
    matches!(
        name,
        "eval"
            | "source"
            | "."
            | "xargs"
            | "sudo"
            | "doas"
            | "su"
            | "exec"
            | "trap"
            | "parallel"
            | "watch"
            | "ssh"
    )
}
fn is_wrapper(name: &str) -> bool {
    matches!(
        name,
        "env" | "nohup" | "time" | "nice" | "timeout" | "command" | "builtin" | "stdbuf" | "ionice"
    )
}
fn is_shell(name: &str) -> bool {
    matches!(name, "sh" | "bash" | "zsh" | "dash" | "ksh")
}
/// Builtins that mvdan/sh parses as declaration clauses; Go reports them as one
/// dynamic `declare` command.
fn is_declaration(name: &str) -> bool {
    matches!(
        name,
        "declare" | "local" | "export" | "readonly" | "typeset" | "nameref"
    )
}

/// Splits a command line into its simple commands. The parser runs on its own thread
/// with a large stack, and a panic in it is an unparsable line, so hostile input can
/// neither crash the daemon nor slip through (D-031).
///
/// # Errors
///
/// [`Error::Unparsable`] when the line cannot be analysed safely.
pub fn split(command: &str) -> Result<Vec<Simple>, Error> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new()
            .name("openchef-shell-parser".to_owned())
            .stack_size(PARSER_STACK)
            .spawn_scoped(scope, || split_depth(command, 0))
            .map_err(|e| unparsable(format!("cannot start the parser: {e}")))?;
        worker
            .join()
            .unwrap_or_else(|_| Err(unparsable("the parser failed on this line")))
    })
}

fn split_depth(command: &str, depth: usize) -> Result<Vec<Simple>, Error> {
    if depth > MAX_DEPTH {
        return Err(unparsable("shells nested too deeply".to_owned()));
    }
    if command.contains('\0') {
        return Err(unparsable("NUL byte".to_owned()));
    }
    if command.len() > MAX_LEN {
        return Err(unparsable(format!("longer than {MAX_LEN} bytes")));
    }
    if too_nested(command) {
        return Err(unparsable(format!("nested deeper than {MAX_NESTING}")));
    }
    let options = ParserOptions::default();
    let mut parser = Parser::new(BufReader::new(command.as_bytes()), &options);
    let program = parser
        .parse_program()
        .map_err(|e| unparsable(e.to_string()))?;
    let mut w = Walker {
        out: Vec::new(),
        depth,
        options,
    };
    for list in &program.complete_commands {
        w.list(list)?;
    }
    Ok(w.out)
}

struct Walker {
    out: Vec<Simple>,
    depth: usize,
    options: ParserOptions,
}

type R = Result<(), Error>;

impl Walker {
    fn list(&mut self, list: &CompoundList) -> R {
        for item in &list.0 {
            self.pipeline(&item.0.first)?;
            for next in &item.0.additional {
                match next {
                    AndOr::And(p) | AndOr::Or(p) => self.pipeline(p)?,
                }
            }
        }
        Ok(())
    }

    fn pipeline(&mut self, p: &Pipeline) -> R {
        for cmd in &p.seq {
            self.command(cmd)?;
        }
        Ok(())
    }

    fn command(&mut self, cmd: &Command) -> R {
        match cmd {
            Command::Simple(s) => self.simple(s),
            Command::Compound(c, redirects) => {
                if let Some(list) = redirects
                    && !list.0.is_empty()
                {
                    let mut s = Simple {
                        args: vec!["(compound)".to_owned()],
                        ..Simple::default()
                    };
                    for r in &list.0 {
                        self.redirect_into(&mut s, r);
                    }
                    self.out.push(s);
                }
                self.compound(c)?;
                if let Some(list) = redirects {
                    for r in &list.0 {
                        self.redirect_substitutions(r)?;
                    }
                }
                Ok(())
            }
            Command::Function(f) => {
                // A function can shadow any command name, which defeats name-based rules.
                self.out.push(Simple {
                    args: vec![format!("function {}", f.fname.value)],
                    dynamic: true,
                    ..Simple::default()
                });
                self.compound(&f.body.0)?;
                if let Some(list) = &f.body.1 {
                    for r in &list.0 {
                        self.redirect_substitutions(r)?;
                    }
                }
                Ok(())
            }
        }
    }

    fn compound(&mut self, c: &CompoundCommand) -> R {
        match c {
            CompoundCommand::Arithmetic(a) => self.substitutions_in_text(&a.expr.value),
            CompoundCommand::ArithmeticForClause(f) => {
                for e in [&f.initializer, &f.condition, &f.updater]
                    .into_iter()
                    .flatten()
                {
                    self.substitutions_in_text(&e.value)?;
                }
                self.list(&f.body.list)
            }
            CompoundCommand::BraceGroup(b) => self.list(&b.list),
            CompoundCommand::Subshell(s) => self.list(&s.list),
            CompoundCommand::ForClause(f) => {
                for w in f.values.iter().flatten() {
                    self.word_substitutions(&w.value)?;
                }
                self.list(&f.body.list)
            }
            CompoundCommand::CaseClause(c) => {
                self.word_substitutions(&c.value.value)?;
                for item in &c.cases {
                    for p in &item.patterns {
                        self.word_substitutions(&p.value)?;
                    }
                    if let Some(cmd) = &item.cmd {
                        self.list(cmd)?;
                    }
                }
                Ok(())
            }
            CompoundCommand::IfClause(i) => {
                self.list(&i.condition)?;
                self.list(&i.then)?;
                for e in i.elses.iter().flatten() {
                    if let Some(c) = &e.condition {
                        self.list(c)?;
                    }
                    self.list(&e.body)?;
                }
                Ok(())
            }
            CompoundCommand::WhileClause(w) | CompoundCommand::UntilClause(w) => {
                self.list(&w.0)?;
                self.list(&w.1.list)
            }
            CompoundCommand::Coprocess(c) => self.command(&c.body),
            CompoundCommand::ExtendedTest(t) => self.test_expr(&t.expr),
        }
    }

    fn test_expr(&mut self, e: &ExtendedTestExpr) -> R {
        match e {
            ExtendedTestExpr::And(a, b) | ExtendedTestExpr::Or(a, b) => {
                self.test_expr(a)?;
                self.test_expr(b)
            }
            ExtendedTestExpr::Not(a) | ExtendedTestExpr::Parenthesized(a) => self.test_expr(a),
            ExtendedTestExpr::UnaryTest(_, w) => self.word_substitutions(&w.value),
            ExtendedTestExpr::BinaryTest(_, a, b) => {
                self.word_substitutions(&a.value)?;
                self.word_substitutions(&b.value)
            }
        }
    }

    fn simple(&mut self, cmd: &SimpleCommand) -> R {
        let prefix = cmd.prefix.as_ref().map_or(&[][..], |p| &p.0[..]);
        let suffix = cmd.suffix.as_ref().map_or(&[][..], |s| &s.0[..]);
        let mut s = Simple::default();
        for item in prefix.iter().chain(suffix) {
            if let CommandPrefixOrSuffixItem::IoRedirect(r) = item {
                self.redirect_into(&mut s, r);
            }
        }
        // Only assignments before the command name change its environment; brush also
        // parses `NAME=value` arguments (env FOO=1 …, export X=…) as assignments.
        let assigns = prefix
            .iter()
            .any(|i| matches!(i, CommandPrefixOrSuffixItem::AssignmentWord(..)));
        // The words: name, then suffix words and process substitutions in order.
        let mut words: Vec<WordOrProc<'_>> = Vec::new();
        if let Some(name) = &cmd.word_or_name {
            words.push(WordOrProc::Word(&name.value));
        }
        for item in suffix {
            match item {
                CommandPrefixOrSuffixItem::Word(w) => words.push(WordOrProc::Word(&w.value)),
                CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                    words.push(WordOrProc::Proc(&sub.list));
                }
                CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                    words.push(WordOrProc::Word(&w.value));
                }
                CommandPrefixOrSuffixItem::IoRedirect(_) => {}
            }
        }

        if words.is_empty() {
            if !s.writes.is_empty() || !s.reads.is_empty() || s.dynamic {
                s.args = vec!["(assignment)".to_owned()];
                self.out.push(s);
            }
        } else {
            if assigns {
                // FOO=x cmd: variables like PATH or LD_PRELOAD change what cmd does.
                s.dynamic = true;
            }
            for w in &words {
                match w {
                    WordOrProc::Word(text) => {
                        let Some(lit) = self.literal(text) else {
                            s.dynamic = true;
                            break;
                        };
                        s.args.push(lit);
                    }
                    WordOrProc::Proc(_) => {
                        s.dynamic = true;
                        break;
                    }
                }
            }
            if s.args.first().is_some_and(|n| is_declaration(n)) {
                self.out.push(Simple {
                    args: vec!["declare".to_owned()],
                    dynamic: true,
                    ..Simple::default()
                });
            } else if s.args.is_empty() {
                self.out.push(s);
            } else {
                let cmds = self.unwrap(s)?;
                self.out.extend(cmds);
            }
        }

        // Then what Go's walk finds inside: assignment values, words, redirect targets.
        for item in prefix {
            if let CommandPrefixOrSuffixItem::AssignmentWord(_, w) = item {
                self.word_substitutions(&w.value)?;
            }
        }
        for w in &words {
            match w {
                WordOrProc::Word(text) => self.word_substitutions(text)?,
                WordOrProc::Proc(list) => self.list(list)?,
            }
        }
        for item in prefix.iter().chain(suffix) {
            if let CommandPrefixOrSuffixItem::IoRedirect(r) = item {
                self.redirect_substitutions(r)?;
            }
        }
        Ok(())
    }

    fn unwrap(&self, mut s: Simple) -> Result<Vec<Simple>, Error> {
        let name = s.args[0].rsplit('/').next().unwrap_or("").to_owned();
        if is_opaque(&name) {
            s.dynamic = true;
            return Ok(vec![s]);
        }
        if is_shell(&name) {
            let Some(script) = shell_script(&s.args[1..]) else {
                if s.args.len() > 1 {
                    s.dynamic = true; // a script file or stdin: contents unknown
                }
                return Ok(vec![s]);
            };
            if s.dynamic {
                return Ok(vec![s]);
            }
            let mut inner = split_depth(&script, self.depth + 1)?;
            for c in &mut inner {
                c.writes.extend(s.writes.iter().cloned());
                c.reads.extend(s.reads.iter().cloned());
            }
            return Ok(inner);
        }
        if is_wrapper(&name) {
            let rest = skip_wrapper_options(&name, &s.args[1..]);
            if rest.is_empty() {
                return Ok(vec![s]);
            }
            return self.unwrap(Simple {
                args: rest,
                dynamic: s.dynamic,
                writes: s.writes,
                reads: s.reads,
            });
        }
        if name == "find"
            && s.args.iter().any(|a| {
                matches!(
                    a.as_str(),
                    "-exec" | "-execdir" | "-ok" | "-okdir" | "-delete"
                )
            })
        {
            s.dynamic = true;
        }
        Ok(vec![s])
    }

    /// Records a redirection target as a write or read (Go `redirects`).
    fn redirect_into(&self, s: &mut Simple, r: &IoRedirect) {
        match r {
            IoRedirect::File(_, kind, target) => {
                let word = match target {
                    IoFileRedirectTarget::Filename(w) | IoFileRedirectTarget::Duplicate(w) => {
                        Some(&w.value)
                    }
                    IoFileRedirectTarget::Fd(_) => None,
                    IoFileRedirectTarget::ProcessSubstitution(..) => {
                        s.dynamic = true;
                        return;
                    }
                };
                let Some(word) = word else { return };
                let lit = self.literal(word);
                match kind {
                    IoFileRedirectKind::Write
                    | IoFileRedirectKind::Append
                    | IoFileRedirectKind::Clobber
                    | IoFileRedirectKind::ReadAndWrite => match lit {
                        None => s.dynamic = true,
                        Some(t)
                            if matches!(
                                t.as_str(),
                                "/dev/null" | "/dev/stdout" | "/dev/stderr"
                            ) => {}
                        Some(t) => s.writes.push(t),
                    },
                    IoFileRedirectKind::Read => match lit {
                        None => s.dynamic = true,
                        Some(t) => s.reads.push(t),
                    },
                    IoFileRedirectKind::DuplicateInput | IoFileRedirectKind::DuplicateOutput => {
                        if let Some(t) = lit
                            && !is_fd(&t)
                        {
                            s.writes.push(t);
                        }
                    }
                }
            }
            IoRedirect::OutputAndError(w, _) => match self.literal(&w.value) {
                None => s.dynamic = true,
                Some(t) if matches!(t.as_str(), "/dev/null" | "/dev/stdout" | "/dev/stderr") => {}
                Some(t) => s.writes.push(t),
            },
            // Here-documents and here-strings feed stdin; no file is touched.
            IoRedirect::HereDocument(..) | IoRedirect::HereString(..) => {}
        }
    }

    fn redirect_substitutions(&mut self, r: &IoRedirect) -> R {
        match r {
            IoRedirect::File(
                _,
                _,
                IoFileRedirectTarget::Filename(w) | IoFileRedirectTarget::Duplicate(w),
            )
            | IoRedirect::HereString(_, w)
            | IoRedirect::OutputAndError(w, _) => self.word_substitutions(&w.value),
            IoRedirect::File(_, _, IoFileRedirectTarget::ProcessSubstitution(_, sub)) => {
                self.list(&sub.list)
            }
            IoRedirect::File(_, _, IoFileRedirectTarget::Fd(_)) => Ok(()),
            IoRedirect::HereDocument(_, doc) => {
                if doc.requires_expansion {
                    let pieces = brush_parser::word::parse_heredoc(&doc.doc.value, &self.options)
                        .map_err(|e| unparsable(e.to_string()))?;
                    self.pieces_substitutions(&pieces)?;
                }
                Ok(())
            }
        }
    }

    /// The value of a word that needs no expansion at run time (Go `literal`).
    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "brush-parser's enum: any piece not listed, today or in a later version, needs expansion and fails closed"
    )]
    fn literal(&self, word: &str) -> Option<String> {
        let pieces = brush_parser::word::parse(word, &self.options).ok()?;
        let mut out = String::new();
        for (i, p) in pieces.iter().enumerate() {
            match &p.piece {
                WordPiece::Text(t) => {
                    if t.contains(['*', '?', '[']) {
                        return None; // a glob expands at run time
                    }
                    out.push_str(t);
                }
                WordPiece::EscapeSequence(e) => {
                    if e.contains(['*', '?', '[']) {
                        return None;
                    }
                    out.push_str(e);
                }
                WordPiece::TildeExpansion(brush_parser::word::TildeExpr::Home) if i == 0 => {
                    out.push('~');
                }
                WordPiece::SingleQuotedText(t) => out.push_str(t),
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    for q in inner {
                        match &q.piece {
                            WordPiece::Text(t) | WordPiece::EscapeSequence(t) => out.push_str(t),
                            _ => return None,
                        }
                    }
                }
                _ => return None,
            }
        }
        Some(out)
    }

    /// Splits the command substitutions found in a word (Go's walk into words).
    fn word_substitutions(&mut self, word: &str) -> R {
        let pieces = brush_parser::word::parse(word, &self.options)
            .map_err(|e| unparsable(e.to_string()))?;
        self.pieces_substitutions(&pieces)
    }

    fn pieces_substitutions(&mut self, pieces: &[WordPieceWithSource]) -> R {
        for p in pieces {
            match &p.piece {
                WordPiece::CommandSubstitution(text)
                | WordPiece::BackquotedCommandSubstitution(text) => {
                    let inner = split_depth(text, self.depth)?;
                    self.out.extend(inner);
                }
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    self.pieces_substitutions(inner)?;
                }
                WordPiece::ArithmeticExpression(e) => self.substitutions_in_text(&e.value)?,
                WordPiece::ParameterExpansion(_) => {
                    // ${x:-$(cmd)} hides commands inside the expansion; this port does not
                    // model parameter expansions, so it fails closed.
                    let raw = format!("{:?}", p.piece);
                    if raw.contains("$(") || raw.contains('`') {
                        return Err(unparsable(
                            "command substitution inside a parameter expansion".to_owned(),
                        ));
                    }
                }
                WordPiece::Text(_)
                | WordPiece::SingleQuotedText(_)
                | WordPiece::AnsiCQuotedText(_)
                | WordPiece::TildeExpansion(_)
                | WordPiece::EscapeSequence(_) => {}
            }
        }
        Ok(())
    }

    /// Arithmetic text can hold `$(…)`; parse it as a word to find them.
    fn substitutions_in_text(&mut self, text: &str) -> R {
        if text.contains("$(") || text.contains('`') {
            self.word_substitutions(text)?;
        }
        Ok(())
    }
}

enum WordOrProc<'a> {
    Word(&'a str),
    Proc(&'a CompoundList),
}

/// The `-c` argument of a shell invocation.
fn shell_script(args: &[String]) -> Option<String> {
    for (i, a) in args.iter().enumerate() {
        if a == "-c" && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        if a.starts_with('-') && a.contains('c') && !a.starts_with("--") && i + 1 < args.len() {
            return Some(args[i + 1].clone()); // -ec, -xc …
        }
        if !a.starts_with('-') {
            return None;
        }
    }
    None
}

/// Drops a wrapper's own options (Go `skipWrapperOptions`).
fn skip_wrapper_options(name: &str, args: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if name == "env" && a.contains('=') && !a.starts_with('-') {
            i += 1;
        } else if a.starts_with('-') {
            i += 1;
            let takes_value = (name == "nice" && a == "-n")
                || (name == "timeout" && matches!(a, "-s" | "-k" | "--signal" | "--kill-after"))
                || (name == "env" && matches!(a, "-u" | "-C" | "-S"))
                || (name == "ionice" && matches!(a, "-c" | "-n"));
            if takes_value {
                i += 1;
            }
        } else if name == "timeout" {
            return args.get(i + 1..).map_or_else(Vec::new, <[String]>::to_vec);
        } else {
            return args[i..].to_vec();
        }
    }
    Vec::new()
}

fn is_fd(s: &str) -> bool {
    s == "-" || (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}
