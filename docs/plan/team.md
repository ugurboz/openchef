# How the team works

## Roles

| Who | Machine | Role |
|---|---|---|
| **Lead (Claude)** | — | Owns `docs/architecture.md`, the decision log and task cards. Writes the cross-cutting core (workspace, domain, process supervision, runtime contract, engine, daemon). Reviews and merges every PR. |
| **Uğur** | macOS | Maintainer. Config, CLI, permissions, router, Codex adapter. Relays questions the lead cannot see. |
| **Elif** | macOS | Store, git workspaces, agy adapter, macOS sandbox, quota, TUI. |
| **İbo** | Windows | Everything Windows: process control, socket, sandbox, live CLI checks, keep-awake, Windows packaging. The only person who can verify Windows behaviour on a real machine. |

## One-time setup (each person)

1. Install Rust with rustup (<https://rustup.rs>). Windows: also the "Desktop development
   with C++" workload of Visual Studio Build Tools.
2. `cargo install cargo-nextest cargo-deny --locked`
3. Clone this repository and the Go reference next to it:
   `git clone --branch go-reference https://github.com/ugurboz/openchef-go ../openchef-go`
4. Use Claude Code (or Codex/agy) in the repository root. It reads `AGENTS.md`
   automatically; do not paste the rules again.
5. Agent CLIs for spikes and live tests: `claude`, `codex`, `agy`, logged in. A CLI you
   do not have is "not tested" in your findings, never guessed.

## Taking a task

1. Pick only issues assigned to you, in the order of the roadmap; respect *Depends on*.
2. Move the issue to *In progress* (comment "started").
3. Start a fresh agent session and paste the card's **Prompt for your agent** block.
4. Stay in the loop: when the agent asks a question it cannot answer from the sources,
   put it on the issue instead of answering from memory.
5. Open the PR when the local gate is clean. The lead reviews; fix what the review asks
   in the same branch. The lead merges.

## Rules that protect `main`

- Branch protection: PR required, CI green on Linux/macOS/Windows, lead approval,
  branch up to date. No force pushes.
- One issue per PR. Scope listed on the card is a boundary.
- If `main` moves under your branch: `git fetch && git merge origin/main`, never rebase
  a pushed branch.

## Communication

- Questions, blockers and design doubts: comment on the issue. The lead answers in the
  next lead session (Uğur starts it).
- Decisions are written down: small ones in the issue, architecture-level ones as a
  decision log entry by the lead.
