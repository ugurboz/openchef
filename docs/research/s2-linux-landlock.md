# S2 — Linux sandbox: Landlock + seccomp (findings)

Spike for task card R1.2 (`docs/architecture.md` §17 item 7, §21.3). Date: 2026-11-27.
Harness and script: `spikes/linux-landlock/`; runs: workflow `spike-linux.yml` on PR #17
(two rounds; round 2 added the resolver path, a private `TMPDIR` and full error output).

## Environment

| Runner | Kernel | Landlock ABI | Ruleset (V5 requested, best effort) | `apparmor_restrict_unprivileged_userns` |
|---|---|---|---|---|
| ubuntu-22.04 x86_64 | 6.8.0-1070-azure | 4 | PartiallyEnforced | 0 |
| ubuntu-24.04 x86_64 | 7.0.0-1020-azure | 8 | FullyEnforced | 1 |
| ubuntu-24.04 arm64 | 7.0.0-1017-azure | 8 | FullyEnforced | 1 |

Agent CLIs: `claude` 2.1.332, `codex` 0.172.0 (npm, not logged in). agy: not installed on
Linux runners — not tested.

## Method

The harness (`landlock-spike run --ro … --rw … -- CMD`) confines **itself** with
Landlock (filesystem) and a seccomp deny list, then `exec`s the command. This is the shape
the product would use (`openchef __sandbox`): the policy is applied in a helper before
`exec`, so no non-async-signal-safe code runs in `pre_exec`.

Policy under test: read-only `/usr /bin /sbin /lib /lib64 /etc /proc /sys /dev /opt
/usr/local ~/.rustup ~/.cargo` (+ `/run/systemd/resolve` in round 2); read-write the
worktree, a private temp dir and `/dev/null`; nothing else. seccomp denies `ptrace`,
`mount`, `umount2`, `pivot_root`, `bpf`, `kexec_load`, `init_module`, `finit_module`,
`delete_module`, `keyctl`, `add_key`, `request_key`, `unshare`, `setns`,
`perf_event_open`, `swapon`, `reboot` with `EPERM`.

## Results (identical on all three runners unless noted)

| Experiment | Expected | Actual | Note |
|---|---|---|---|
| Read a secret in `$HOME` | fail | fail | `Permission denied` |
| Write outside the worktree (`$HOME`) | fail | fail | `Permission denied` |
| Write the shared `/tmp` | fail | fail | `Permission denied` |
| List another directory in `$HOME` | fail | fail | `Permission denied` |
| Write the worktree / private temp / `/dev/null` | ok | ok | |
| Read a system file, `git status` in the worktree | ok | ok | |
| `unshare --user` confined (seccomp) | fail | fail | `Operation not permitted` |
| `mount -t tmpfs` confined | fail | fail | needs root anyway |
| `unshare --user --map-root-user` **unconfined** | — | ok on 22.04, **fails on 24.04** | AppArmor restricts unprivileged user namespaces on 24.04 |
| HTTPS to `api.anthropic.com`, `registry.npmjs.org` | ok | **round 1: fail** → round 2: ok | `/etc/resolv.conf` is a symlink into `/run/systemd/resolve`; Landlock checks the target |
| `cargo test` with default `TMPDIR=/tmp` | ok | **fail** | the linker aborts (`linking with cc failed: SIGABRT`): it cannot create temp files |
| `cargo test` with a private `TMPDIR` (default or private `CARGO_HOME`) | ok | ok | crate without dependencies |
| `go test` with default caches | ok | **fail** | `mkdir ~/.cache: permission denied` |
| `go test` with `GOCACHE`/`GOPATH` private but `/tmp` | ok | **fail** | `creating work dir: mkdir /tmp/go-build…` |
| `go test` with caches and `TMPDIR` private | ok | ok | |
| `npm test` (default or private cache) | ok | ok | project without dependencies |
| `python3 -m unittest` | ok | ok | `__pycache__` lands in the worktree |
| `claude --version`, `codex --version` | ok | ok | start-up needs no write outside the policy |
| `claude -p hi` without login | fail (auth) | fail (auth) | a normal "not logged in" result, no `EPERM` crash |
| `systemd-inhibit --what=idle:sleep` (S5) | — | `Access denied` | the runner user has no logind session (`XDG_SESSION_ID` unset) |

## Conclusions

1. **Landlock + seccomp enforce the §21.3 file policy on every tested kernel**, including
   ABI 4 (Ubuntu 22.04 HWE). Escapes are blocked by the kernel, not by the agent.
2. **User namespaces cannot be part of the design**: Ubuntu 24.04 blocks unprivileged
   ones by default. Landlock and seccomp need no privileges and no namespaces. There is
   therefore no PID namespace; process-tree control stays with `openchef-procs`.
3. **Every allowed path must be resolved through symlinks** when the policy is built
   (`/etc/resolv.conf` → `/run/systemd/resolve/stub-resolv.conf`), otherwise DNS breaks.
4. **Every sandboxed process gets a private `TMPDIR`**; compilers and linkers fail without
   one. Tool caches must be redirected or made writable: Go needs `GOCACHE`/`GOPATH`
   (`GOMODCACHE`), npm `npm_config_cache`, Cargo `CARGO_HOME` once dependencies are
   fetched, pip `PIP_CACHE_DIR`.
5. **Network is not confined** (v1 policy). Landlock ABI ≥ 4 can restrict TCP `bind`/
   `connect` by port only, which is not an egress allowlist; domain-based egress stays
   post-v1 (§22).
6. **Keep-awake** via `systemd-inhibit` needs a logind session; it was denied on CI. It
   must be checked by `openchef doctor` on a real desktop and reported when unavailable.

## Recommendation for R4.6

- Implement the sandbox as a hidden helper subcommand (`openchef __sandbox <policy.json>
  -- CMD…`) that applies Landlock then seccomp to itself and `exec`s; adapters and the
  verification runner start agents and commands through it.
- Landlock: request ABI V5 best effort; **fail closed** when the kernel reports no
  Landlock at all or the ruleset comes back `NotEnforced` (`--sandbox` refuses, `doctor`
  explains). `PartiallyEnforced` (older ABI) is accepted and reported.
- Policy builder: system read-only set as above plus canonicalized symlink targets; the
  worktree, a per-session temp dir and `/dev/null` read-write; `TMPDIR` set to the temp
  dir; the agent CLI's own state directory read-write (`~/.claude`, `~/.codex`, the agy
  directory) — the exact set is **not yet verified with a logged-in turn on Linux**.
- **Caches (decision for the lead):** per-repository caches under
  `~/.openchef/sandbox-cache/<repo-id>/` (Go, npm, Cargo, pip) by default, set through
  the tools' environment variables. They survive across tasks of one repository but do
  not share — and cannot poison — the user's global caches. `[sandbox] writable` can
  opt in to the global caches for speed.
- seccomp: the deny list above, `EPERM`, per architecture (x86_64, aarch64).

## Not tested (needs a Linux machine with logged-in CLIs)

- A full logged-in turn of `claude`, `codex`, `agy` under the policy, including token
  refresh and the CLIs' own sandboxes (Codex uses Landlock/seccomp itself; nesting
  Landlock is allowed by the kernel, but the combined behaviour is unmeasured).
- Pseudo-terminals (`/dev/ptmx`, `/dev/pts`) for tools that need a TTY.
- Projects with dependencies (registry downloads into redirected caches).
- `systemd-inhibit` in a desktop session, on battery and with the lid closed.
