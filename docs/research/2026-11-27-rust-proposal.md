> **Status (2026-11-27): partially accepted.** Rust: accepted (D-027). Built-in sandbox
> without Docker: accepted as an OS-native sandbox (D-028); the micro-VM and credential
> proxy moved after v1 (architecture §22). Bounded debate: accepted with frugal defaults
> (D-029). Corrections: M3 and M4 were already built in Go when this was written; Go can
> sandbox as well (runc, gVisor, Lima), so the sandbox was not the deciding factor. Kept
> for the record; the architecture document is the source of truth.

# OpenChef ADR & Architecture Evolution: Transitioning to 100% Rust, Built-in Virtualization, and Autonomous Multi-Agent Debate

| Field | Value |
|---|---|
| **Document Type** | Architectural Decision Record (ADR) & v1.0 Target Specification Brief |
| **Status** | Maintainer Decision — Supersedes language choice & §7.5 / §21.3 of `docs/architecture.md` (v0.6) |
| **Target Audience** | Lead Architect / Implementation Agents (Claude Opus, Codex, Antigravity) |
| **Repository** | `github.com/ugurboz/openchef` |

---

## 1. Executive Context: Why Re-evaluate the Stack Now?

In `docs/architecture.md` (v0.6), **Go** was selected over the earlier **Python** prototype (`unified-ai`) to achieve single-binary portability, deterministic process supervision (`internal/procs`), race-tested JSON-RPC adapters (`internal/runtime/{claude,codex,agy}`), and CGO-free SQLite persistence (`internal/store`). Under the original assumptions—human-paced coding velocity and delegating `--sandbox` isolation to external Docker/Podman installations (§21.3)—Go was a sound engineering choice.

However, **four foundational axioms of OpenChef have changed** by maintainer decision:

1. **Strict Zero External Runtime Dependency (No Docker):** OpenChef must **not** depend on Docker Desktop, Podman, OrbStack, Colima, Python, Node, or any external virtualization daemon. Every capability—including full isolation—must ship self-contained. This invalidates `docs/architecture.md` §21.3.
2. **Built-in Toggleable Virtualization (`--vm` / `--virtualize`):** When the user enables virtualization for an overnight run, the execution environment where agent CLIs and project toolchains run must be hardware/kernel-virtualized directly via native OS hypervisors, eliminating host compromise and credential exfiltration risks completely.
3. **Overnight Multi-Agent Debate & Autonomous Convergence:** Rather than a strictly linear `Plan → Implement → Passive Review` pipeline (§7.5, where review findings are not even auto-fixed in v1), OpenChef's core differentiator is an **autonomous brigade** that debates architecture, critiques implementations across providers, runs kernel verification, fixes defects iteratively, survives quota resets, and delivers a finished, verified project by morning.
4. **Zero Sunk-Cost Constraint + AI-Driven Implementation:** Rewriting the existing M0–M3 Go code (~35% of v1) is explicitly acceptable. Code authoring is performed by AI coding agents; engineering bottlenecks have shifted from *typing speed* to *compile-time correctness guarantees* and *systems-level isolation primitives*. Meanwhile, all empirical protocol discoveries (`docs/research/m1-findings.md`, D-006…D-017) and golden JSONL transcripts (`testdata/`) transfer 1:1.

**Decision:** Migrate OpenChef to **100% Rust** before building M3–M8, replacing §21.3 with a native hypervisor/kernel sandbox subsystem and upgrading §7.5 with a bounded multi-agent adversarial debate engine.

---

## 2. Three-Language Architectural Comparison (Python vs. Go vs. Rust)

### 2.1 Addressing Previous Proposals Head-On

- **Why Python (proposed by Gemini Neon) is disqualified:**
  Python excelled as a rapid Phase 0–6 research vehicle (`Orchestrator` / `unified-ai`), proving L1/L2 quota models, ACP/MCP flows, and TUI ergonomics. As a widely distributed multi-OS systems product with zero external dependencies and built-in virtualization, Python fails structurally: Python itself is a fragile runtime dependency (`uv`/`venv`/wheels), cannot embed native VMM/hypervisor or `pre_exec` kernel sandboxes without C/Rust extensions, and already exhibited process leaks and `asyncio` lifecycle fragility in the prototype (`docs/architecture.md` §5.2).

- **Why Go (proposed by Claude Opus) was right for v0.6, and where it hits friction now:**
  Go thrives in pure I/O control planes (`gh`, `lazygit`, Docker CLI) and delivered `CGO_ENABLED=0` static builds via `modernc.org/sqlite` plus clean POSIX shell parsing via `mvdan.cc/sh/v3`. Under the expanded scope, two advantages evaporate and two liabilities emerge:
  1. **`CGO_ENABLED=0` disappears anyway for native virtualization:** Calling Apple's `Virtualization.framework` (`Code-Hex/vz`) or `Hypervisor.framework` on macOS requires Objective-C/C bridges (`CGO_ENABLED=1`). Once CGO enters the build matrix, Go loses its pure cross-compilation edge over Rust.
  2. **Multi-threaded runtime vs. low-level Linux sandboxing:** Go's M:N goroutine scheduler spawns OS threads before `main()`, making direct post-`fork` namespace unsharing (`CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWNET`), `Landlock`, and `seccomp-bpf` unsafe without `runc`-style self-re-exec C-constructor workarounds.
  3. **VMM ecosystem gravity:** Modern micro-VM infrastructure (**Firecracker**, **`rust-vmm`**, Red Hat **`libkrun`**, **Cloud Hypervisor**) and modern agent sandboxes (**OpenAI `codex-rs`**) are overwhelmingly native Rust. Bridging Go to `libkrun` reintroduces a dual-language FFI/IPC split with zero upside once Go sunk cost is discounted.
  4. **Runtime vs. compile-time verification for AI-authored code:** When AI agents generate complex concurrent state machines (`Task`, `Debate`, `Session`, `Quota`, `VmInstance`), Go defers `nil` dereferences, unhandled enum variants, and channel/lock races to runtime tests (`-race`). Rust rejects them at compile time.

### 2.2 Comparative Matrix Across OpenChef Subsystems

| Architectural Dimension | Python | Go | Rust (Selected) |
|---|---|---|---|
| **Zero-Dependency Distribution (Goal G5)** | Fails (requires runtime or brittle bundler) | Strong (`CGO_ENABLED=0`, lost on macOS VM APIs) | **Strongest** (native per-OS CI builds; ~4 MB idle RAM; zero GC) |
| **Built-in Micro-VM Hypervisor (No Docker)** | Impossible natively | Possible via CGO (`vz`) + `hcsshim` + KVM wrappers | **Industry Standard** (`libkrun`, `rust-vmm`, `objc2-virtualization`, `windows-sys` WHPX/HCS) |
| **OS Kernel Sandbox (`Landlock`, `seccomp`, Seatbelt, `AppContainer`)** | Requires external helper binary | Requires self-re-exec pattern due to Go runtime threads | **Native** (single-threaded `CommandExt::pre_exec` + `landlock` + `seccompiler` + `windows-sys`) |
| **Process Tree Kill Domains (`procs`)** | Fragile (`ctypes`/`pywin32`) | Proven (`kqueue`, `Pdeathsig`, Win32 Job Objects) | **Equivalent Precision** (`nix`, `kqueue`, `windows-sys` with RAII `Drop` cleanup) |
| **State Machine & Protocol Type Safety** | Runtime + `mypy`/`Pydantic` | Structs/interfaces; no sum types or exhaustive switches | **Strongest** (Algebraic `enum`s, `serde` tagged unions, compiler-enforced exhaustive `match`) |
| **AI-Assisted Development Reliability** | High runtime bug escape rate | Fast authoring, moderate runtime panic/race escape rate | **Highest Signal** (`rustc` + strict `clippy` act as an automated formal gate for AI code) |
| **POSIX Shell AST Security (§9.2)** | Weak (`bashlex` unmaintained) | Excellent (`mvdan.cc/sh/v3`) | **Strong** (`brush-parser` pure-Rust POSIX/Bash AST + strict fail-closed fallback) |
| **Embedded SQLite Persistence (§11)** | `sqlite3` stdlib | `modernc.org/sqlite` (transpiled C, ~2× slower) | **`rusqlite` (`bundled`)** (canonical C SQLite statically linked, WAL, fast) |
| **TUI Responsiveness & Ecosystem (§12.2)** | `Textual` | `Bubble Tea` | **`ratatui` + `crossterm`** (proven at scale by `codex-rs`) |

---

## 3. Core Architectural Additions Beyond `docs/architecture.md` v0.6

### 3.1 Built-in Two-Tier Isolation (Replacing §21.3 Docker Backend)

OpenChef must provide deterministic safety **without requiring Docker or any third-party daemon**:

#### Tier 1 — OS-Native Kernel Sandbox (`--sandbox`, instant startup, zero image download)
Used for standard local tasks and as a defense-in-depth wrapper around `verify` commands and agent processes:
- **macOS:** Dynamic Seatbelt profile (`sandbox_init` / `/usr/bin/sandbox-exec`, built into macOS) with an isolated `$HOME` and `$TMPDIR` under `~/.openchef/sandboxes/<task-id>/`. Denies reads/writes to the user's real home directory (`~/Documents`, `~/.ssh`, `~/.aws`, `~/.gnupg`, Keychain) while allowing read-only access to system/toolchain paths (`/usr`, `/bin`, `/opt/homebrew`, `~/.cargo`, `~/.nvm`) and read-write access strictly inside the task worktree.
- **Linux:** Unprivileged `CLONE_NEWUSER | CLONE_NEWNS | CLONE_NEWPID` namespaces + `Landlock` LSM + `seccomp-bpf` syscall filter applied in `pre_exec`.
- **Windows:** `JobObject` (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) + `AppContainer` / Restricted Token capability SIDs confining writes to the task worktree.

#### Tier 2 — Built-in Hardware Micro-VM (`--vm` / `--virtualize`, full overnight isolation)
Used when the user enables virtualization for unattended overnight multi-agent project builds where agents may install arbitrary packages (`npm`, `pip`, `cargo`, `apt`), bind local ports, run databases, and execute arbitrary scripts with `bypassPermissions` inside the guest:
- **Hypervisor Backends (Zero Docker):**
  - **macOS (Apple Silicon & Intel):** Apple `Virtualization.framework` (`objc2-virtualization`) or `libkrun` (`Hypervisor.framework`), booting a lightweight Linux guest kernel + rootfs in < 1 second.
  - **Linux:** `/dev/kvm` via `libkrun` / `rust-vmm` (with explicit user-consented fallback to Tier 1 namespace+Landlock only if KVM is unavailable, e.g., nested cloud VMs).
  - **Windows:** Windows Hypervisor Platform (`WHPX`) / Host Compute Service (`HCS` via `windows-sys`).
- **On-Demand Signed Rootfs Management:**
  - On first `--vm` invocation, `openchef` downloads a minimal, reproducible OpenChef Linux rootfs image (`~/.openchef/vm/images/<version>.img`), verifies its SHA-256 and Cosign signature, and creates a per-task **copy-on-write overlay disk** (`~/.openchef/vm/instances/<task-id>.cow`).
  - **No host bind-mounts:** The user's repository is cloned into the guest's block volume over `virtio-vsock` (git bundle/fetch). Nothing on the host filesystem is ever mounted into the guest.
  - On completion, `openchef` fetches only verified git commits/branches out of the guest via `virtio-vsock` into `openchef/<task-id>` on the host and destroys the COW disk.

#### Critical Security Invariant — Host Credential Injection Proxy (Zero Token Exfiltration)
Running `claude`, `codex`, or `agy` *inside* a guest VM creates a subtle attack vector: if raw OAuth/API credentials are injected into the guest VM, a malicious dependency installed at 03:00 AM (`postinstall` script or prompt injection) cannot harm the host OS, **but could exfiltrate the user's subscription OAuth token over the network**.

OpenChef eliminates this vector via **Host-Side Credential Mediation**:
1. **Never write real OAuth tokens or API keys to the guest disk or guest environment.**
2. All guest outbound traffic routes through a user-space network stack / `virtio-vsock` HTTP/TLS proxy inside the host `openchef daemon` (`hyper` + `rustls`), restricted to an egress domain allowlist (configurable; defaults to model APIs, GitHub, crates.io, npmjs, PyPI, Go proxy).
3. Agent CLIs inside the guest receive synthetic placeholder credentials (`OPENCHEF_PROXY_TOKEN`). Only when connecting to verified provider API endpoints (`api.anthropic.com`, `chatgpt.com` / `api.openai.com`, Google endpoints) does the host proxy swap the placeholder header for the real credential held exclusively in host daemon memory.
4. *M1-R Spike Requirement:* Verify per-CLI base-URL / proxy header compatibility (`ANTHROPIC_BASE_URL`, `OPENAI_BASE_URL`, `agy` proxy settings). For any CLI that pins TLS certificates and refuses base-URL override, keep the **CLI process itself on the host** inside Tier 1 Seatbelt/Landlock while forwarding all its tool executions (`Bash`, file edits, builds, servers) into the Tier 2 guest VM over `virtio-vsock`. Either topology guarantees **zero raw credentials inside the untrusted build environment**.

---

### 3.2 Autonomous Multi-Agent Debate & Convergence Engine (Upgrading §7.5)

To let agents collaborate and debate overnight until a production-grade project emerges—**without burning weekly quotas in infinite polite loops**—OpenChef replaces unstructured chat with a **Bounded Adversarial State Machine**:

```
User Prompt
    │
    ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 1: Architecture & Plan Council (Bounded: max 3 rounds)         │
│  1. Proposer (Frontier Agent, Pool A) ──► Drafts Spec + Step Plan    │
│  2. Critics (Frontier/Standard Agents, Pools B & C, parallel)        │
│     ──► Structured Critique: { severity: Blocker|Major|Nit, ... }    │
│  3. Convergence Gate:                                                │
│     • 0 Blockers & 0 Majors ──► Lock Plan                            │
│     • Remaining Blockers/Majors ──► Proposer revises (next round)    │
│     • Round limit hit ──► Arbiter synthesis (or User Gate if attended)│
└──────────────────────────────┬───────────────────────────────────────┘
                               │
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 2: Per-Step Implement–Verify–Debate Loop (Isolated / Team)     │
│  1. Implementer (Pool X) writes code & tests inside VM/Worktree      │
│  2. Kernel Verification Gate: OpenChef runs build + test + lint      │
│     • Fails ──► Feed exact compiler/test output back (same session)  │
│  3. Cross-Provider Adversarial Review (Pool Y ≠ Pool X):             │
│     • Emits schema-validated findings (Blocker | Major | Nit)        │
│     • Blockers/Majors > 0 ──► Re-open Implementer turn to fix/rebut  │
│       (if Implementer rebuts with evidence, Arbiter Pool Z decides)  │
│       [Bounded: max 3 review-fix rounds per step]                    │
│     • Nits only ──► Recorded in Morning Report; DO NOT burn turns    │
│  4. Commit verified step; advance merge queue                        │
└──────────────────────────────┬───────────────────────────────────────┘
                               │
                               ▼
┌──────────────────────────────────────────────────────────────────────┐
│ Phase 3: Whole-Project Integration Audit & Morning Report            │
│  Full test suite + cross-provider final audit + `openchef report`    │
└──────────────────────────────────────────────────────────────────────┘
```

**Anti-Runaway Circuit Breakers (Mandatory):**
1. **Severity Gating:** Stylistic `Nit` findings never trigger another LLM turn. Every debate turn must cite concrete file/line/contract defects (`Blocker` or `Major`).
2. **Stagnation Detection:** If two consecutive fix iterations produce an identical verification failure hash or oscillating diff hash, the engine halts the loop on that agent and routes a single escalation turn to a different frontier pool (or marks the step `needs_attention` while independent `team` steps continue).
3. **Quota-Aware Endurance:** If all eligible pools hit `exhausted` at 02:30 AM, `openchef daemon` checkpoints state to SQLite, pauses the VM (`pause` / snapshot), sleeps until the earliest `resets_at` timestamp (e.g., 05:00 AM), and resumes autonomously before the user wakes up.

---

## 4. Target Rust Workspace & Crate Architecture

To keep incremental `cargo check` / `cargo test` times under 2–3 seconds and prevent AI coding agents from fighting Rust's borrow checker, the codebase **must** enforce two structural rules from commit #1:

### 4.1 Golden Engineering Rules for AI-Authored Rust

1. **Actor / Message-Passing Concurrency (No `Arc<Mutex<...>>` Graphs):**
   Every stateful subsystem (`DaemonCore`, `TaskRunner`, `DebateRoom`, `SessionActor`, `QuotaTracker`, `ApprovalQueue`, `VmInstance`) runs inside its own `tokio::spawn` task owning its state exclusively, communicating via strongly typed, `'static` owned `enum` commands over bounded `tokio::sync::mpsc` and `oneshot` channels. Avoid shared mutable state and lifetime-parameterized async traits.
2. **Strict Workspace Compiler Gates (`Cargo.toml` `[workspace.lints]`):**
   ```toml
   [workspace.lints.rust]
   unsafe_code = "deny" # Explicitly overridden ONLY in openchef-procs and openchef-vm
   unused_must_use = "deny"

   [workspace.lints.clippy]
   unwrap_used = "deny"
   expect_used = "deny"
   todo = "deny"
   unimplemented = "deny"
   dbg_macro = "deny"
   exhaustive_enums = "warn"
   ```
3. **Zero Silent Failures:** Typed errors per crate via `thiserror`; rich context boundaries at CLI/daemon edges; every raw CLI protocol line written to `raw.jsonl` *before* `serde_json` deserialization.

### 4.2 Cargo Workspace Layout

```text
openchef/
├── Cargo.toml                  # Workspace root, shared dependencies, strict clippy/rust lints
├── crates/
│   ├── openchef-core/          # Pure domain types & state machines (Task, Step, Attempt, Debate,
│   │                           # Quota math, Config/Settings structs). Zero I/O dependencies.
│   ├── openchef-store/         # SQLite WAL persistence (`rusqlite` bundled), single-writer actor
│   │                           # + read pool, forward-only SQL migrations, upgrade fixtures.
│   ├── openchef-procs/         # Cross-platform leak-free process supervision:
│   │                           # Unix pgrp + Linux Pdeathsig + macOS kqueue NOTE_EXIT/sysctl
│   │                           # + Windows JobObject/NtResumeProcess (`#[allow(unsafe_code)]`).
│   ├── openchef-permissions/   # Claude-compatible rule syntax, `brush-parser` POSIX shell AST
│   │                           # decomposition, deterministic risk classifier (routine/critical).
│   ├── openchef-sandbox/       # Tier 1 OS-native sandboxing (macOS Seatbelt, Linux Landlock +
│   │                           # seccomp + namespaces, Windows AppContainer) + Host Credential Proxy.
│   ├── openchef-vm/            # Tier 2 zero-Docker Micro-VM manager (`objc2-virtualization` /
│   │                           # `libkrun` / KVM / WHPX), signed rootfs cache, COW disk, vsock RPC.
│   ├── openchef-runtime/       # `Runtime` & `Session` traits, normalized `Event` enum, `linerpc`
│   │                           # bidirectional JSON-RPC 2.0 peer, `rawlog`, `ask_user` stdio MCP,
│   │                           # adapters (`claude`, `codex`, `agy`), and scriptable `fake` CLI.
│   ├── openchef-gitws/         # External git worktrees (`~/.openchef/worktrees`), selective staging,
│   │                           # trailers, binary failover patches, `team` rebase-verify merge queue.
│   ├── openchef-engine/        # Step runner, Bounded Adversarial Debate council, quota router,
│   │                           # stagnation detector, crash recovery.
│   ├── openchef-daemon/        # Single-instance lock, Unix/Windows AF_UNIX JSON-RPC server,
│   │                           # keep-awake (`caffeinate` / `systemd-inhibit` / Win32), scheduler.
│   ├── openchef-tui/           # `ratatui` + `crossterm` client (task stream, debate view, modals).
│   └── openchef-cli/           # Binary crate (`openchef`): `clap` CLI + TUI + `daemon` + internal
│                               # MCP/sandbox entrypoints in a single executable.
├── docs/
│   ├── architecture.md         # Source-of-truth specification (to be amended to v1.0-rust)
│   └── research/               # Preserved M1 spike findings (`m1-findings.md` + VM proxy spike)
└── testdata/                   # Preserved golden JSONL recordings (`claude/`, `codex/`, `agy/`)
```

### 4.3 Selected Crate Dependencies

| Concern | Crate(s) | Rationale |
|---|---|---|
| Async runtime & IPC | `tokio` | Process pipes, timers, Unix sockets, `mpsc`/`oneshot` actors |
| Serialization & Schemas | `serde`, `serde_json`, `toml`, `schemars`, `jsonschema` | Compile-time JSON Schema generation + strict validation |
| Persistence | `rusqlite` (`features = ["bundled"]`) | Self-contained C SQLite with WAL; zero host dependency |
| Shell AST parsing | `brush-parser` | Pure-Rust POSIX/Bash parser preventing command-chaining bypasses |
| HTTP / Credential Proxy | `hyper`, `tokio-rustls`, `rustls` | Pure-Rust TLS & L2 quota probes / VM egress credential proxy |
| OS syscalls | `nix`, `libc` (Unix), `windows-sys` (Windows) | Direct syscall control for `procs`, `sandbox`, and `vm` |
| Micro-VM / Sandboxing | `objc2-virtualization` / `libkrun`, `landlock`, `seccompiler` | Native Docker-free virtualization across macOS & Linux |
| CLI & TUI | `clap`, `ratatui`, `crossterm` | Industry-standard Rust CLI/TUI stack (same foundation as `codex-rs`) |
| Testing | `insta`, `proptest`, `tempfile` | Snapshot/golden replay tests + state-machine property testing |

---

## 5. What Must Be Preserved Verbatim from the Current Repository

Do **not** re-discover or discard the empirical engineering already completed in `openchef`:

1. **`docs/research/m1-findings.md` & Decisions D-006 through D-017:**
   - Claude `--restricted`, `--permission-prompt-tool stdio` control protocol, `result_index` turn correlation, `result.usage` single-counting, and `dontAsk` + `Read,Glob,Grep` planner mode (avoiding Claude's `plan`-mode silent model switch).
   - Codex `app-server` JSON-RPC flow, `thread/start` / `thread/resume`, `item/*/requestApproval`, `mcpServer/elicitation/request`, `codexErrorInfo`, and `account/rateLimits/*`.
   - Antigravity `agy acp` with `--sandbox --cli_dir_name openchef`, `proceed-in-sandbox` escape handling, `session/load` history-replay dropping, and temp-dir refusal.
2. **`testdata/{claude,codex,agy}/*.jsonl` Golden Fixtures:**
   Use these exact recordings as the replay test suite for `openchef-runtime` in Rust.
3. **Core Domain Contracts (`docs/architecture.md` §§2, 4, 5, 6, 8, 9, 10, 11):**
   Write-ahead persistence before side effects, kernel-only verification authority, pool-aware failover (never failing over to another model on the same exhausted pool), importance vs. adaptive effort profiles, and clean environment allowlisting.

---

## 6. Phased Execution Roadmap (For Opus & Implementation Agents)

| Milestone | Scope | Exit Criteria |
|---|---|---|
| **M0-R: Spec Amendment & Rust Foundation** | Amend `docs/architecture.md` (Rust stack, §7.5 Debate Engine, §21.3 Built-in Tier 1/2 Virtualization + Credential Proxy). Initialize Cargo workspace, strict lints, 3-OS GitHub Actions CI (`cargo clippy`, `cargo test`, `cargo deny`), `openchef-core`, `openchef-procs`, `openchef-store`, `doctor` & `init`. | Green CI on Linux, macOS, Windows; leak-free process supervision tests pass on all 3 OSes; SQLite WAL migrations pass. |
| **M1-R: Virtualization & Credential Proxy Spike** | Verify: (1) booting minimal Linux guest via Apple `Virtualization.framework` / `libkrun` + `virtio-vsock` exec/git-bundle without Docker; (2) Host Credential Proxy vs. host-CLI/guest-tool split for `claude`, `codex`, `agy`. Record findings in `docs/research/m1r-vm-findings.md`. | Zero-Docker guest command execution + zero raw credential exposure inside guest proven end-to-end. |
| **M2-R: Runtimes & Replay Parity** | Implement `linerpc`, `rawlog`, `ask_user` stdio MCP server, `fake` CLI binary, and `claude`, `codex`, `agy` adapters in `openchef-runtime`. | 100% of existing `testdata/` golden replay tests + live 1-turn & resume smoke tests pass. |
| **M3-R: Daemon, Git Workspaces & Single-Step Engine** | `openchef-gitws` (`isolated` + `team` merge queue), `openchef-daemon` (JSON-RPC socket), step execution loop, verification gate, failover patch handoff, `waiting_quota` auto-resume, live CLI event streaming (`run`, `logs -f`, `status`). | Kill-9 daemon recovery test passes with zero leaked processes and intact step resumption. |
| **M4-R: Permissions, Quota & Tier 1 OS Sandbox** | `openchef-permissions` (`brush-parser` shell AST + rule evaluator + approval queue), L1/L2/L3 quota admission & adaptive effort, `openchef-sandbox` (Seatbelt / Landlock+seccomp / AppContainer). | Chained shell bypasses (`pytest; curl … \| sh`) rejected; out-of-workspace writes blocked at OS kernel level. |
| **M5-R: Multi-Agent Debate & Autonomous Pipeline** | Phase 1 Architecture Council + Phase 2 Adversarial Implement-Verify-Review loop, stagnation circuit breakers, cross-provider routing, unattended deferred queue & Morning Report. | Multi-agent overnight pipeline completes a multi-step project autonomously across ≥2 providers with simulated quota failover. |
| **M6-R: Built-in Micro-VM Backend (`--vm`)** | `openchef-vm` production integration: signed on-demand rootfs fetch, per-task COW disk, `vsock` bridge, Host Credential Proxy, `export` / `merge`. | Full `--vm --unattended` overnight build installs packages, runs servers/tests inside guest VM with zero Docker installed and zero host filesystem/token exposure. |
| **M7-R: `ratatui` TUI & v1.0 Release** | Interactive TUI (task/debate live stream, quota meters, focused approval modals, slash commands), `cargo-dist` / GitHub Releases + Homebrew tap + Cosign signatures. | `brew install` / standalone archive delivers single `openchef` binary across macOS, Linux, Windows. |
