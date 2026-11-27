# Roadmap — OpenChef v1 in Rust

Milestones R0–R8 from `docs/architecture.md` §19, broken into task cards with an owner,
dependencies and the Go reference each one ports. Cards for the current milestones are
written out in full in `docs/plan/tasks/` and opened as GitHub issues. Cards for later
milestones are written by the lead when the milestone starts, because their exact APIs
depend on what earlier milestones produce; the owners below are the plan, not a promise.

**People:** Lead = Claude (architecture, task cards, review, merges, and the cross-cutting
core), **Uğur** (macOS), **Elif** (macOS), **İbo** (Windows). Linux runs in CI.

Size: S ≈ one session, M ≈ 2–3 sessions, L ≈ a week of sessions.

## R0 Foundation

| Card | Title | Owner | Depends on | Go reference | Size |
|---|---|---|---|---|---|
| [R0.1](tasks/R0.1-workspace-ci.md) | Cargo workspace, lints, CI on 3 OSes, templates | Lead | — | `.github/`, `.golangci.yml` | M |
| [R0.2](tasks/R0.2-core-domain.md) | `openchef-core`: domain model and state machines | Lead | R0.1 | `internal/task` | M |
| [R0.3](tasks/R0.3-procs-unix.md) | `openchef-procs`: API + macOS/Linux supervision | Lead | R0.1 | `internal/procs` | L |
| [R0.4](tasks/R0.4-procs-windows.md) | `openchef-procs`: Windows Job Objects | İbo | R0.3 | `internal/procs/*windows*` | M |
| [R0.5](tasks/R0.5-store.md) | `openchef-store`: SQLite, migrations, repositories | Elif | R0.2 | `internal/store` | L |
| [R0.6](tasks/R0.6-config.md) | `openchef-config`: TOML config, paths, validation | Uğur | R0.1 | `internal/config` | M |
| [R0.7](tasks/R0.7-cli-doctor-init.md) | `openchef-cli`: clap skeleton, `version`, `doctor`, `init` | Uğur | R0.6 | `internal/doctor`, `internal/buildinfo`, `internal/cli`, `cmd/openchef` | M |
| [R0.8](tasks/R0.8-release-dry-run.md) | `cargo-dist` release dry run in CI | İbo | R0.7 | `.goreleaser.yaml` | S |

Exit: CI green on 3 OSes; leak tests pass on 3 OSes; `openchef doctor` detects the
installed CLIs.

## R1 Spikes (run in parallel with R0)

| Card | Title | Owner | Depends on | Size |
|---|---|---|---|---|
| [R1.1](tasks/R1.1-spike-macos-seatbelt.md) | S1 macOS Seatbelt (+ S5 keep-awake on macOS) | Elif | — | M |
| [R1.2](tasks/R1.2-spike-linux-landlock.md) | S2 Linux Landlock + seccomp (+ S5 on Linux) | Lead | R0.1 | M |
| [R1.3](tasks/R1.3-spike-windows.md) | S3 Windows: CLIs, Job Objects, AF_UNIX, confinement (+ S5 on Windows) | İbo | — | L |
| [R1.4](tasks/R1.4-spike-shell-parser.md) | S4 `brush-parser` against the Go bypass corpus | Uğur | R0.1 | M |

Exit: findings merged in `docs/research/`; the lead amends §9.2 and §21.3 with the results.

## R2 Runtimes (cards written at R2 start)

| Card | Title | Owner | Go reference |
|---|---|---|---|
| R2.1 | Runtime contract, events, `linerpc`, `rawlog`, turn queue | Lead | `internal/runtime`, `linerpc`, `rawlog`, `turnq` |
| R2.2 | `openchef-fake` scriptable fake CLI | Lead | `internal/runtime/*/fake_test.go` |
| R2.3 | Claude adapter (D-006…D-013) | Lead | `internal/runtime/claude` |
| R2.4 | Codex adapter (D-003…D-005) | Uğur | `internal/runtime/codex` |
| R2.5 | agy adapter (D-015…D-017) | Elif | `internal/runtime/agy` |
| R2.6 | Windows live verification of all three adapters; Windows CI fixes | İbo | — |

Exit: every `testdata/` replay passes; one turn + resume live on each runtime.

## R3 Engine

| Card | Title | Owner | Go reference |
|---|---|---|---|
| R3.1 | `openchef-gitws`: worktrees, commits, patches, reset | Elif | `internal/gitws` |
| R3.2 | `openchef-api` + `openchef-daemon`: lock, socket, start/stop/status | Lead | `internal/api`, `internal/daemon` |
| R3.3 | Daemon socket on Windows (AF_UNIX + ACL) | İbo | `internal/daemon` (Windows parts) |
| R3.4 | Verification runner | Uğur | `internal/verify` (on `go-reference`) |
| R3.5 | Engine: step loop, retries, failover, recovery | Lead | `internal/engine` |
| R3.6 | Service + CLI `run`/`status`/`logs -f`/`cancel`/`retry`, live renderer, kill-the-daemon e2e | Lead + Uğur (CLI renderer) | `internal/service`, `internal/cli`, `internal/e2e` |

## R4 Permissions, quota, selection, sandbox

| Card | Title | Owner |
|---|---|---|
| R4.1 | Rules, layered settings, evaluator, shell parsing (from S4) | Uğur |
| R4.2 | Approval queue, modes, `ask_user` MCP server | Lead |
| R4.3 | Quota: windows, merge rules, health, L1/L2/L3, probes, kill switch | Elif |
| R4.4 | Router: tiers, effort, strategies; admission | Uğur |
| R4.5 | `openchef-sandbox` macOS (from S1) | Elif |
| R4.6 | `openchef-sandbox` Linux (from S2) | Lead |
| R4.7 | `openchef-sandbox` Windows (from S3) | İbo |
| R4.8 | Sandbox wiring into adapters and verification; `doctor` checks | Lead |

## R5 Pipeline and debate

| Card | Title | Owner |
|---|---|---|
| R5.1 | Planner, plan schema, repair turn, approval with edits | Lead |
| R5.2 | Findings schema, reviewer, review → fix loop | Lead |
| R5.3 | Plan council, arbiter, disputed findings | Uğur |
| R5.4 | Circuit breakers: stagnation, quota, budget | Lead |
| R5.5 | `team` mode, merge queue, `merge` | Elif |
| R5.6 | Integration audit + fake-CLI multi-provider e2e | İbo |

## R6 TUI — Elif (lead UI), Uğur (slash commands, approvals modal), İbo (Windows terminal parity)

## R7 Unattended — risk classes and deferred queue (Uğur), keep-awake (İbo), report (Elif), sandbox defaults and overnight e2e (Lead)

## R8 Release — `cargo-dist` + Homebrew tap (Uğur), winget/Scoop + Windows signing (İbo), docs and examples (Elif), security notes and v1.0.0 (Lead)
