# S4 — Shell parser: `brush-parser` against the Go corpus (findings)

Spike for task card R1.4 (`docs/architecture.md` §9.2, §17 item 9; D-021). Date:
2026-11-27. Code: `spikes/shell-parser/` (Rust splitter, comparison report, tests) and
`spikes/shell-parser/oracle/` (the Go implementation's `shell.go`, verbatim from tag
`go-reference`, behind a JSON CLI).

## Versions

`brush-parser` 0.5.0 (default `ParserOptions`, bash mode) · Go oracle: `mvdan.cc/sh/v3`
v3.14.1, Go 1.27.

## Method

1. **Corpus** (`corpus-inputs.json`, 147 lines): every input of the Go `TestSplit`,
   `TestBypassCorpus`, `TestBashWildcardAndPrefix` and `FuzzSplit` seeds, plus 85 hostile
   additions (ANSI-C quoting, brace expansion, `{ …; }` groups, arithmetic with
   substitutions, `coproc`, aliases, line continuations, Unicode look-alikes and
   zero-width characters, NUL bytes, 5 000-character words, five-deep substitutions,
   nested `sh -c`, here-documents with and without expansion, here-strings,
   assignments with substitutions, declaration builtins, loops, `case`, `[[ ]]`, `time`,
   `!`, background jobs, every redirection operator, process substitution as a
   redirection target, every wrapper and opaque runner, `bash` with and without a
   script, `find -delete`, `$"…"`, `|&`, nested backquotes, function forms, `select`).
2. **Expected values come from running Go**: `oracle` prints the Go `Split` result for
   every line (`corpus.json`). Nothing was transcribed by hand.
3. **Rust splitter** (`src/lib.rs`): the Go rules ported onto the `brush-parser` AST and
   its word parser (`brush_parser::word::parse`), walking every word, assignment,
   redirection target, here-document and test expression for command substitutions.
4. **Comparison**: `cargo run --release` writes `report.md`; `tests/compare.rs` fails on
   any difference that is not documented below.
5. **Fuzzing**: `tests/fuzz.rs` (proptest) — 300 000 random lines from a shell
   metacharacter alphabet plus arbitrary Unicode, release and debug builds: no panic,
   none slower than 1 s, no empty static command (the Go `FuzzSplit` invariants).
   Deep-nesting probes: `src/bin/deep.rs`, `src/bin/smallstack.rs`.

## Results

**142 of 147 identical.** The first run found 8 differences; 3 were bugs in the port
(fixed, below) and 5 remain, all on the safe side:

| Input | Go | Rust | Verdict |
|---|---|---|---|
| `()0` | `function` dyn ; `0` | unparsable | **fail closed** — malformed input mvdan tolerates |
| `echo hi\0rm` | `echo hirm` | unparsable | **fail closed** — NUL bytes are rejected |
| `echo ${x:-$(rm y)}` | `echo` dyn ; `rm y` | unparsable | **fail closed** — parameter-expansion words are not modelled yet (fix in R4.1 if it matters in practice) |
| `> out.log` | *(nothing)* | `(assignment)` writes `out.log` | **Rust correct, Go bug** — a bare redirection truncates the file (`> ~/.bashrc`); the Go splitter reported no command at all |
| `select x in a; do rm; done` | `rm` | unparsable | **fail closed** — brush-parser has no `select` |

Bugs found in the port during the spike (all fixed, now covered by the comparison test):
`NAME=value` arguments after the command (`env FOO=1 …`, `alias ls='…'`) were taken for
prefix assignments, and **`export FOO=$(curl evil)` lost the `curl evil` command**: a deny
rule for `curl` would not have fired. The corpus caught it on the first run, which is
the point of running Go as the oracle.

Hostile cases worth recording (identical in both): Unicode look-alikes (`ｒｍ`) and
zero-width joins are just other command names (not allowed by an `rm` rule, not denied
either: allow-lists, not deny-lists, are the safe default); `((ls))` is arithmetic and
runs no command; brace expansion `{a,b}` is a literal word in both; heredoc bodies with
`$(…)` are split, quoted heredocs (`<<'EOF'`) are not.

## Blocker found: stack overflow on deep nesting

`brush-parser` recurses on the native stack. A line with ~5 000 nested `{ … }` **overflows
the stack and aborts the process** (`fatal runtime error: stack overflow`), which Rust
cannot catch. An agent could crash the daemon with one command. Go grows its stacks and
does not have this problem.

Mitigation, implemented in the spike and measured:

- Before parsing, reject lines longer than **64 KiB** and lines nested deeper than
  **64** (a quote-blind count of `(`/`{` depth and of backquotes; over-counting only makes
  it stricter). Rejected lines are unparsable → **ask** (and `critical` unattended).
- 64 is safe on a **2 MiB** thread stack (Tokio's worker default) in **debug** builds;
  128 still overflowed there. Real commands need far less (the corpus maximum is 5).
- Belt and braces for R4.1: run the parser on a dedicated thread with an 8 MiB stack.

Timings: 5 000-command pipelines parse in ~6 ms; rejected lines in < 30 µs.

## Recommendation for R4.1

**Use `brush-parser`**, with:

1. The splitter from `spikes/shell-parser/src/lib.rs` as the starting point, and its
   corpus, oracle output and comparison test moved into `openchef-permissions` tests
   (the oracle stays in the spike as the regeneration tool).
2. The length and nesting guard (64 KiB, depth 64) **before** parsing, and parsing on a
   dedicated 8 MiB-stack thread.
3. Fail closed on: parse errors, NUL bytes, `select`, command substitutions inside
   parameter expansions, the guard limits — all become "ask".
4. Keep Rust's stricter answer for a bare redirection (`> file` is a write) and record it
   as a deliberate change from the Go behaviour.
5. Pin `brush-parser` to an exact version; a parser upgrade must re-run the corpus
   comparison (CI job) before it is accepted.
