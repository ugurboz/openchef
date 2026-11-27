# S1 — macOS sandbox: Seatbelt (findings)

Spike for task card R1.1 (`docs/architecture.md` §17 item 6, §21.3; S5 keep-awake on
macOS). Date: 2026-11-27. Profile and scripts: `spikes/macos-seatbelt/`
(`policy.sb`, `run.sh basic|verify`, `sbx.sh` for agent runs).

## Environment

macOS 27.2.1 (26B131), Apple Silicon · Claude Code 2.1.326 (logged in, claude.ai) ·
Codex CLI 0.172.0 (logged in, ChatGPT) · agy 1.5.18 · `man sandbox-exec`: **"The
sandbox-exec command is DEPRECATED"** — it still works on this release.

## Policy under test (`policy.sb`)

`(allow default)`, then: **writes** denied everywhere except the worktree, a private
`TMPDIR`, the agent CLI's state directories, `/dev/null`, `/dev/zero`, `/dev/ptmx`,
`/dev/tty*`, `/dev/fd/*` and Claude Code's `/private/tmp/claude-<pid>-cwd`; **reads**
denied under `$HOME` except the worktree, temp dir, CLI state, `~/.local`, `~/.cargo`,
`~/.rustup`, `~/.nvm`, `~/go`, `~/.gitconfig` and `~/Library/Keychains`. Network open.
Parameters are passed with `sandbox-exec -f policy.sb -D NAME=value`; later rules win
over earlier ones (deny then re-allow works as written).

Starting from `(deny default)` was not tried further: Node- and Rust-based CLIs touch many
mach services, sysctls and devices; the threat in §21.3 is files, and file rules are
what the profile enforces.

## Results

### File policy (plain commands, `run.sh basic`)

| Experiment | Expected | Actual |
|---|---|---|
| Read a fake secret in `$HOME` / list `~/Documents` | fail | fail — `Operation not permitted` |
| Write `$HOME`, the shared `/private/tmp`, `/usr/local` | fail | fail — `Operation not permitted` |
| Write the worktree / private `TMPDIR` / `/dev/null` | ok | ok |
| Read `/etc/hosts`, `git status` in the worktree | ok | ok |
| HTTPS to `api.anthropic.com` | ok | ok |
| `sandbox-exec` **inside** the sandbox | ok | **fail — `sandbox_apply: Operation not permitted`** |

### Verification commands (`run.sh verify`)

`cargo test`, `npm test` (default or private cache) and `python3 -m unittest` pass with
a private `TMPDIR`. `go test` fails with default caches (`mkdir ~/Library: Operation not
permitted`) and passes with `GOCACHE`/`GOPATH` in the temp dir — the same as Linux (S2).

### Agent CLIs (real, logged-in, cheapest model, one turn each)

| CLI | Result |
|---|---|
| `claude -p` (haiku), no state dir | **works** — login comes from the Keychain through `securityd`; no file access needed |
| `claude -p` with `~/.claude` + `~/.claude.json` writable | works; **`--resume` works** (remembered "KESTREL"); with state read-only the session is not found |
| Claude Bash tool | needs `/private/tmp/claude-<uid>/` (writable state, independent of `TMPDIR`) and `/private/tmp/claude-<pid>-cwd` |
| **Escape attempt**, Claude `--dangerously-skip-permissions` asked to `cat` the secret and write `$HOME` | **both blocked by the kernel** (`Operation not permitted`); writing the worktree works |
| `codex exec --sandbox workspace-write` | **every command fails**: Codex applies its own Seatbelt profile, which cannot nest (`sandbox_apply`, exit 71) |
| `codex exec --sandbox danger-full-access` inside ours | works; worktree write ok, secret read and `$HOME` write **blocked by our profile** |
| `agy --sandbox -p` | its nested Seatbelt fails (exit 71) and **agy retries the command without its sandbox** on its own; our profile still blocked the secret read and the `$HOME` write; worktree write ok |
| agy | needs `~/.gemini` writable and `/dev/ptmx` (it runs commands on a PTY) |

Gemini's content filter rejected a prompt containing "secret" and "escape"; neutral file
names were used for agy (`oc-notes.txt`, `oc-out.txt`). That is a property of the model,
not of the sandbox.

### Keep-awake (S5, macOS)

`caffeinate -i -w <pid>` holds a `PreventUserIdleSystemSleep` assertion for the life of
`<pid>` (seen in `pmset -g assertions`). Lid-closed and battery behaviour: **not tested**
(needs a person at the machine).

## Conclusions

1. **Seatbelt enforces the §21.3 file policy on all three CLIs**, including an agent that
   has every permission prompt switched off. Escapes are stopped by the kernel.
2. **Seatbelt does not nest.** A CLI's own sandbox cannot run inside OpenChef's. When
   OpenChef's sandbox is on, OpenChef's sandbox *is* the sandbox: Codex must run with
   `danger-full-access` (approvals still go to OpenChef through app-server), and agy's
   sandbox fails and falls back by itself.
3. **agy silently runs a command unsandboxed when its sandbox cannot be applied.** Outside
   OpenChef's sandbox that fallback has not been seen, but it means agy's own sandbox is
   not a guarantee: D-015 ("sandbox always on") must not be counted as a security
   boundary without OpenChef's.
4. Per-CLI writable state (macOS):

   | CLI | Writable | Notes |
   |---|---|---|
   | claude | `~/.claude/`, `~/.claude.json`, `/private/tmp/claude-<uid>/`, `/private/tmp/claude-<pid>-cwd` | Keychain via mach (no file rule) |
   | codex | `~/.codex/` | |
   | agy | `~/.gemini/` | `/dev/ptmx` |

5. `sandbox-exec` is deprecated but functional; the API it wraps (`sandbox_init` with a
   profile string) is what Codex and Claude Code use too. Inline profiles with `-p` work
   outside a sandbox.

## Recommendation for R4.5

- Generate the profile per session (worktree, temp dir, CLI state from the table above)
  and start agents and verification commands as `sandbox-exec -p <profile> -D … -- cmd`
  from `openchef-procs` (the sandbox wraps the process; procs still owns the tree).
- Always set `TMPDIR` to the private temp dir and redirect tool caches (S2 decision: per-
  repository caches under `~/.openchef/sandbox-cache/<repo-id>/`).
- With `--sandbox`: Codex `danger-full-access` (approval policy unchanged), agy unchanged
  but documented as unconfined by itself; amend D-003 and D-015 in R4.5.
- `openchef doctor` checks that `sandbox-exec` exists and that a trivial profile applies;
  if not, `--sandbox` fails closed.
- Watch Apple's deprecation: if `sandbox-exec` disappears, call `sandbox_init` directly
  (the private-but-stable API Codex uses).

## Not tested

Lid-closed / battery keep-awake; `codex app-server` and `agy acp` transports (smoke tests
used `codex exec` and `agy -p`; the confinement is per process, so the transport should
not change the result — to confirm in R4.5); projects with dependencies (registry
downloads into redirected caches); older macOS versions.
