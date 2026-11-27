# AGENTS.md — rules for every agent working in this repository

You are working on **OpenChef**, a local orchestrator for AI coding agents (Claude Code,
Codex, Antigravity), written in Rust. Contributors drive you through task cards; a lead
reviewer (also an agent) reviews every pull request. Follow these rules exactly. If a
rule and the task card disagree, stop and ask on the issue.

## 1. Sources of truth (read before you write code)

1. Your **task card**: the GitHub issue you were given (also in `docs/plan/tasks/`). Its
   *Scope* list is a hard boundary.
2. `docs/architecture.md`: what OpenChef does. Read the sections your card links.
3. `docs/decisions/log.md`: why. D-001…D-026 come from the Go implementation and still
   bind behaviour; D-027+ are Rust-era rules.
4. **The Go reference** (`ugurboz/openchef-go`, tag `go-reference`): the behavioural
   spec for every ported crate. Clone it next to this repo
   (`git clone --branch go-reference https://github.com/ugurboz/openchef-go ../openchef-go`)
   and read the Go package and its tests your card names.
5. `docs/research/` and `testdata/`: recorded protocol findings and transcripts. Never
   edit `testdata/` files by hand.

Do not invent behaviour that none of these describe. Ask instead (§7).

## 2. Workflow for one task card

1. `git switch main && git pull`, then `git switch -c <your-name>/<issue>-<slug>`
   (example: `elif/12-store-migrations`).
2. Read the card, the linked architecture sections and the Go reference files.
3. **Port the tests first.** Bring over every test of the named Go package (same
   names in `snake_case`, same cases), see them fail, then write the code that makes
   them pass. A Go test you drop needs a one-line reason in the PR description.
4. Run the full local gate (§4) until it is clean.
5. Commit in small, logical commits (Conventional Commits, English:
   `feat(store): …`, `fix(procs): …`, `test(...)`, `docs(...)`).
6. Push and open a PR with `gh pr create`, using the template. Title like the card,
   body starts with `Closes #<issue>`, then the checklist.
7. Address review comments with new commits on the same branch. Never force-push.

## 3. Rust rules (enforced by CI where possible)

- **Actors, not shared locks (D-030).** A stateful component is one `tokio` task that
  owns its state and receives a typed `enum Command` over a bounded `mpsc` channel, with
  `oneshot` replies. No `Arc<Mutex<…>>` graphs. A `std::sync::Mutex` is allowed only
  for a small leaf value and never held across `.await`.
- **Cancellation is a message.** Long operations take a
  `tokio_util::sync::CancellationToken`. Never rely on dropping a future to stop a
  protocol exchange.
- **Closed enums** for statuses, events, failure kinds, runtimes, findings. Match them
  exhaustively; no `_ =>` arm on our own enums.
- **No panics:** `unwrap`, `expect`, `panic!`, `todo!`, `unimplemented!`, `dbg!` are
  denied outside tests. Return errors.
- **Errors:** one `thiserror` enum per crate (`pub enum Error`), with context in the
  variant fields. `anyhow` only in `openchef-cli`. Never discard a `Result`
  (`let _ = …` needs a comment saying why it is safe).
- **`unsafe`** is denied except in `openchef-procs` and `openchef-sandbox`; every block
  there has a `// SAFETY:` comment.
- **Raw first:** every line read from an agent CLI is written to the session's
  `raw.jsonl` before it is parsed.
- **Dependencies:** only those in the workspace `Cargo.toml`. Adding one needs a reason
  in the PR and must pass `cargo deny check`.
- Keep functions small, names explicit, comments about *why*. Public items get doc
  comments. No commented-out code, no `// ... rest of code`.

## 4. Local gate (run all before every push)

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace
cargo test --workspace --doc
cargo deny check
```

Windows contributors run the same commands in PowerShell. If a test is flaky locally,
it is a bug: fix it, do not retry until green.

## 5. Tests

- Port Go tests first (§2.3). Table-driven cases become `#[test]` functions or
  `proptest`/`rstest`-style tables.
- Tests must not depend on timing margins below 1 s on CI; wait for events, not sleeps.
- `unwrap`/`expect` are fine inside `#[test]` functions. Integration test files
  (`tests/*.rs`) with helper functions start with
  `#![allow(clippy::unwrap_used, clippy::expect_used)]` after the file doc comment.
- Use `tempfile` for every filesystem test; never touch the real `~/.openchef`.
- Live tests against real CLIs are `#[ignore]` and run only with `OPENCHEF_LIVE=1`.
- Platform-specific tests use `#[cfg(target_os = "…")]` and must exist for every OS
  the code supports.

## 6. Git rules

- Never commit to `main`. Never force-push. Never rewrite pushed history.
- Stage explicit paths (`git add crates/openchef-store`), never `git add -A` or `.`.
- Never commit secrets, `.env` files, local paths or generated build output.
- Do not touch files outside your card's *Scope*. Shared files (`Cargo.toml` at the
  root, CI workflows, `AGENTS.md`, `docs/architecture.md`) change only when the card
  says so.

## 7. When you are unsure

Stop and write a comment on the issue: what you found, the options, which one you
recommend and why. Do not guess an architectural decision. Small local choices
(private helper names, test layout) are yours.

## 8. Definition of done

- Every acceptance criterion of the card is met and ticked in the PR.
- Local gate (§4) is clean; CI is green on Linux, macOS and Windows.
- Ported Go tests are listed in the PR, with reasons for any dropped.
- No `TODO` without an issue number.
