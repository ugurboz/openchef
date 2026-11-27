# Decision Log

Engineering decisions taken while implementing `docs/architecture.md`, newest last.
Architecture-level decisions are amended into the architecture document itself; this log
records the reasoning behind implementation choices and how each one was challenged.

From D-006 on, every non-trivial decision is challenged by an independent reviewer agent
before it is taken; the entry records the strongest objection and why the final choice
survives it.

Format: **Context** → **Options** → **Challenge** → **Decision** → **Consequences**.

D-001…D-026 were taken in the Go implementation (`ugurboz/openchef-go`, tag
`go-reference`). Their protocol and behaviour rules stay binding for the Rust rewrite;
where they name Go packages or types, read the matching crate (`internal/<x>` →
`crates/openchef-<x>`). D-027 onwards belong to the Rust rewrite.

---

## D-001 — JSON-RPC notifications use an unbounded ordered queue
*2026-11-25 · M2.1 · `internal/runtime/linerpc`*

- **Context:** the spike client used a fixed 4096-slot channel and dropped notifications
  when it was full. A dropped `turn/completed` hangs a turn forever.
- **Options:** (a) bounded channel and block the reader; (b) bounded and drop;
  (c) unbounded queue between reader and consumer.
- **Challenge:** an unbounded queue can grow without limit if the consumer stalls.
- **Decision:** (c). Blocking the reader (a) also blocks responses to our own calls and
  can deadlock with approval handlers; dropping (b) is a correctness bug. Growth is
  bounded in practice by the agent's own output rate and the session's lifetime, and the
  message size limit (64 MiB) bounds each entry.
- **Consequences:** consumers must drain `Notifications()` until it closes (documented).

## D-002 — Turn events are delivered through a non-blocking queue
*2026-11-25 · M2.2 · `internal/runtime/codex`*

- **Context:** first implementation sent events on the turn channel while holding the
  turn lock. A consumer that cancels and stops reading filled the buffer and wedged the
  router, `Interrupt` and `Close` (found in self-review before any test).
- **Decision:** `emit`/`finish` append to a queue and never block; a dedicated goroutine
  delivers to the channel and closes it after the terminal event.
- **Consequences:** regression test `TestCloseWithUnreadEventsDoesNotBlock` (2000 unread
  events, `Close` must return).

## D-003 — Codex: one sandbox/approval setting for all modes except `plan`
*2026-11-25 · M2.2*

- **Context:** Codex has `approvalPolicy` × `sandbox`, not Claude's six modes.
- **Decision:** every mode except `plan` uses `approvalPolicy: "untrusted"` +
  `sandbox: "workspace-write"`, so **every** request reaches OpenChef's approver, which
  applies the mode semantics and the deny rules. `plan` uses `read-only`.
- **Why:** deny rules must hold in every mode (architecture §9.3); letting Codex
  auto-approve in permissive modes would bypass them, exactly the hole found in Claude.
- **Consequences:** more approval round-trips in permissive modes; the approver answers
  them automatically, so no human is involved.

## D-004 — Codex usage = difference of thread totals, never sums of parts
*2026-11-25 · M2.2*

- **Context:** the prototype double-counted tokens by summing message-level usage.
- **Decision:** per turn, usage = `total` at the last `thread/tokenUsage/updated` minus
  (`total` − `last`) of the first one. Cached input is split out so fields are additive.
- **Consequences:** correct even after `thread/resume`, when the thread total already
  includes earlier turns.

## D-005 — A turn id is learned only from `turn/start` or `turn/started`
*2026-11-25 · M2.2 · found by the live test*

- **Context:** a live run hung: right after `thread/resume`, Codex sent a
  `thread/tokenUsage/updated` for the *previous* turn; the router, slightly behind,
  processed it after the new turn was registered and adopted the old turn id, so every
  real notification of the new turn was discarded.
- **Decision:** the id comes only from the `turn/start` response or `turn/started`.
  Turn-scoped notifications that arrive while it is unknown are held back in order and
  replayed under the router lock once it is known; those of other turns are dropped.
- **Consequences:** regression tests `TestStaleNotificationsAreIgnored` and
  `TestImmediateFailureBeforeTurnStarted` (each repeated 20× per run); live test passed
  3/3 afterwards.

## D-006 — Claude sessions always run with `--restricted` and an explicit tool list
*2026-11-25 · M2.3 · reviewed in two rounds, evidence: m1-findings §9–§10*

- **Context:** without it, a Claude session loads the repository's
  `.claude/settings.local.json` (hooks run arbitrary shell, extra allow rules, env), hooks
  outrank the host, and the default tool set includes tools that act outside the turn
  (`CronCreate`, `RemoteTrigger`, `ScheduleWakeup`, `Monitor`, `Task`, …). In permissive
  modes none of that reaches OpenChef.
- **Options:** (a) `--setting-sources local` as before; (b) an empty setting source
  plus a long `--disallowedTools` list; (c) `--restricted` + `--tools` allowlist.
- **Challenge (reviewer):** "isolation leaks"; `--tools` might strip `StructuredOutput`
  and MCP tools; `--restricted` was unverified in combination.
- **Decision:** (c). Spikes showed `--restricted` ignores settings files (no hook events
  in restricted sessions), confines file tools to the working directory, and sends
  writes to settings/git/tool-config files to the host even in `acceptEdits`; it works
  with `auto`, `dontAsk`, `--resume`, `--mcp-config` and `--json-schema`, and
  `StructuredOutput` survives `--tools`. Allowlist: `Bash, Read, Edit, Write, Glob,
  Grep, NotebookEdit, TodoWrite`. Web tools are off by default and enabled per agent
  (reviewer: exfiltration risk under prompt injection).
- **Consequences:** `Probe` must fail loudly if a CLI version rejects `--restricted`.

## D-007 — Claude rules: native deny *and* ask; invariants as deny rules; approver re-checks
*2026-11-25 · M2.3*

- **Context:** outside `manual`, Claude never consults the host, so OpenChef-side rules
  silently stop applying (reviewer); Bash prefix rules are easy to evade
  (`git -C . push`, `sh -c '…'`).
- **Decision:** pass `permissions.deny` (spec deny rules + invariants such as
  `Write(**/.claude/**)`, `Edit(**/.git/**)`, history-rewriting git) and
  `permissions.ask` (spec ask rules) natively, plus `permissions.allow` for OpenChef's own
  `ask_user` tool. The approver applies the §9.2(1) invariants to every request that does
  reach it (`blocked_path`, `input.file_path`), in every mode. Bash deny rules are
  documented as best-effort; the kernel keeps authority over git (it owns commits and
  checks refs before merging, §8.2).

## D-008 — Claude `bypassPermissions` → `acceptEdits` + approver; `plan` → read-only tools
*2026-11-25 · M2.3*

- **Context:** `--restricted` refuses `bypassPermissions`. Claude's own `plan` mode
  switched `haiku` to `claude-sonnet-5-5` in every test (~25× cost) and writes plan files
  under `$HOME/.claude/plans`.
- **Decision:** `bypassPermissions` runs as `acceptEdits` with the approver allowing
  whatever is not denied by rules or invariants. OpenChef's `plan` mode runs Claude in
  `dontAsk` with `--tools Read,Glob,Grep` — read-only by construction, no model switch.
- **Consequences:** `TurnResult.Models` (from `result.modelUsage`) is reported so any
  future model drift is visible to routing and cost accounting.

## D-009 — Claude turns are correlated by per-process result counting
*2026-11-25 · M2.3*

- **Context:** Claude messages carry no turn id; an interrupted turn's late `result`
  could be credited to the next turn (the D-005 failure mode). Spikes: exactly one
  `result` per user message, `result_index` increments per turn and restarts at 0 in a
  resumed process; interrupt returns `still_queued`.
- **Decision:** the adapter counts user messages written to the current process; a turn
  ends only on the `result` whose `result_index` equals its ordinal. A new turn is
  refused while a result is owed. `Interrupt` blocks until the owed result arrives (or
  10 s pass, then the process is killed and transparently resumed on the next turn).
  Messages reported in `still_queued` are subtracted.

## D-010 — Claude Close kills fast when a turn is in flight
*2026-11-25 · M2.3*

- **Context:** after stdin EOF Claude finishes the turn in flight (8.8 s measured), i.e.
  keeps writing to the worktree while the engine may be committing.
- **Decision:** idle → close stdin, normal grace. In flight → interrupt, wait ≤ 3 s for the
  result, then kill the process group without further grace.

## D-011 — Per-turn output schema by transparent re-spawn
*2026-11-25 · M2.3*

- **Context:** Claude only accepts `--json-schema` at start; the engine sets a schema per
  turn (planner, reviewer).
- **Options:** (a) a session-level schema field; (b) re-spawn with `--resume` when the
  schema changes.
- **Decision:** (b), only between turns, and back without the schema when a later turn
  has none. Cost: one process start and likely a prompt-cache miss on that turn.

## D-012 — Claude: an interrupt that finds the message still queued replaces the process
*2026-11-25 · M2.3 · amends D-009*

- **Context:** D-009 said messages reported in `still_queued` are subtracted from the
  owed-result count. The protocol does not say whether a still-queued message is
  dropped or will still run after the interrupt; if it runs, its late `result` would be
  credited to the next turn (the D-005 failure mode).
- **Options:** (a) subtract and trust that no result follows; (b) wait for a result
  that may never come; (c) kill the process and let the next turn re-spawn it.
- **Challenge (self-review):** killing discards a message the user wanted interrupted
  anyway, so nothing is lost; the only cost is latency on the next turn.
- **Decision:** (c). It is correct under both readings, costs one process start, and
  only happens when an interrupt lands before Claude picked up the message.
- **Consequences:** test `TestInterruptOfQueuedMessageReplacesProcess`.

## D-013 — Claude: `--resume` only after the first result; one fallback on start failure
*2026-11-25 · M2.3 · live check with claude 2.1.325*

- **Context:** a replacement process (D-011 schema change, D-009/D-012 kill, crash)
  continues the conversation with `--resume`. Live check: `--resume` of a session id
  that has no messages yet fails with `No conversation found with session ID`, while
  starting again with the same `--session-id` works. A first turn with an output schema
  hits exactly this case.
- **Challenge (self-review):** "retry with the other flag" could mask real start
  failures. It does not: the retry happens at most once, only for replacement
  processes (never for `Open`/`Resume`), and both errors are returned joined.
- **Decision:** the session counts as started once any process returned a `result`
  (or when the engine calls `Resume`); before that a replacement reuses
  `--session-id`. A process that died mid-turn may or may not have written a
  transcript, so a failed replacement start is retried once with the other flag.
- **Also:** `error_max_budget_usd` is classified `unknown`, not `quota_exhausted`: it
  is a `--max-budget-usd` cap (not passed by OpenChef today), and treating it as quota
  would park the task in `waiting_quota`.
- **Consequences:** tests `TestStructuredOutputRespawnsAndBack`,
  `TestCrashOnFirstTurnRecoversWithFallback`; `TestLive` covers the schema-first path
  against the real CLI. Start failures carry the CLI's stderr tail.

## D-014 — `run` streams what the agents do, live, from M3
*2026-11-25 · plan · requested by the user*

- **Context:** the TUI (M6) is the main place to watch agents, but M3–M5 would otherwise
  run tasks that are visible only through `status` and log files. Watching what an agent
  does is a core need, not a polish item.
- **Options:** (a) wait for the TUI; (b) a separate `watch` command; (c) `run` and
  `pipeline` stream the task's events by default, like `claude` and `codex` do.
- **Challenge (self-review):** streaming in the foreground could tie the task's lifetime
  to the terminal. It does not: the daemon owns the task; the terminal only renders, and
  Ctrl-C detaches the view instead of cancelling.
- **Decision:** (c), with `-q/--quiet`, `--detach` and `--json`. `logs -f` and `run` share
  one renderer over the normalized events (§6.1), so the TUI later reuses the same event
  model.
- **Consequences:** M3 exit criteria include seeing a running task live from its first
  turn.

## D-015 — agy: sandbox always on, `proceed-in-sandbox`, ACP mode always `default`
*2026-11-25 · M2.4 · evidence: m1-findings §11*

- **Context:** agy ignores `permissions.deny` entirely, and its `auto`/`accept-edits`
  modes act without consulting the host, so OpenChef rules would silently stop
  applying. The terminal sandbox, measured, allows only reads inside the workspace: no
  writes, no network, no reads outside.
- **Options:** (a) sandbox on, `proceed-in-sandbox`; (b) sandbox off, `request-review`
  (every command asks once); (c) use agy's own modes for OpenChef's modes.
- **Challenge (self-review):** (a) lets commands run unasked, so deny rules do not see
  them; and builds need writes, so every one escapes and costs a round-trip.
- **Decision:** (a). Unasked commands are contained to reading the workspace, which
  every mode allows anyway; anything that writes, reaches the network or leaves the
  workspace must escape with `BypassSandbox` and therefore reaches the approver, which
  applies the mode, deny rules and invariants. (b) would run approved commands with
  full user rights, weaker than Codex's `workspace-write`. (c) is ruled out by `auto`
  and `accept-edits` bypassing the host. OpenChef's modes are therefore implemented by
  the approver (as for Codex, D-003): agy always runs in ACP mode `default`; `plan`
  denies every request; `acceptEdits` auto-allows edits subject to invariants.
- **Consequences:** settings are written by OpenChef to its own `--cli_dir_name`
  directory (the user's agy configuration is never read). A repository under the temp
  tree loses containment (the sandbox allows `$TMPDIR`): the adapter refuses to start
  there. Approval requests carry `BypassSandbox` so the approver and the UI can show
  that a command leaves the sandbox. agy's `plan` mode is never used (it writes outside
  the workspace).

## D-016 — agy protocol rules: options by kind, swallow load replay, cancel by notification
*2026-11-25 · M2.4*

- **Decision:** permission answers select the option whose `kind` is `allow_once` /
  `reject_once` (ids changed between versions); if none matches, the request is
  cancelled. Updates received while `session/load` runs are history and are dropped
  (the D-005 failure mode). `Interrupt` sends `session/cancel` and waits for the prompt
  to return `cancelled` (kill and reload after 10 s, as for Claude). Usage is the
  prompt result's per-turn `usage`; cached reads are reported separately.
- **Consequences:** replay tests on `m18-load-resumed`, `m18-cancel`, `m18-bypass`.

## D-017 — agy: structured output by prompt, validation and one repair; response barrier
*2026-11-25 · M2.4*

- **Context:** agy has no native output schema (m1-findings §4), and ACP ends a turn with
  the `session/prompt` response, which the JSON-RPC peer delivers on a different path
  than the `session/update` notifications that precede it.
- **Decision:** the adapter appends the schema to the prompt, extracts the JSON value
  from the final answer (whole text, last fenced block, or outermost object/array) and
  validates it with `internal/schema` (`santhosh-tekuri/jsonschema/v6`, pure Go,
  Apache-2.0; external `$ref`s refused). One repair prompt follows a failure; a second
  failure ends the turn as `protocol`. Usage counts both prompts. The final answer is
  the agent text after the last tool call. `linerpc.Peer.Barrier` makes the adapter wait
  until every update read before the response was routed, so no chunk is lost or
  credited to the next turn.
- **Challenge (self-review):** a new dependency for one runtime. It is small, has no
  transitive network or cgo use, and the engine can reuse it to double-check native
  schema output.
- **Consequences:** tests `TestStructuredOutput`, `TestBarrierKeepsEveryChunk`,
  `TestBarrierOrdersNotificationsBeforeResponse`.

## D-018 — Domain model in `internal/task`; store transitions are compare-and-set
*2026-11-25 · M3.3*

- **Context:** the engine (M3.5), recovery (M3.7) and the CLI all need the task, step,
  attempt, session and turn types and the §7.1 state machine; the store must enforce the
  write-ahead rule (§11) when the daemon, a CLI command (`cancel`, `retry`) and recovery
  race for the same row.
- **Options:** (a) state machine inside `internal/engine`, store writes whatever it is
  given; (b) a dependency-free domain package that the store and engine both import, and
  every store transition guarded by the caller's expected state.
- **Challenge (self-review):** (b) adds a package not in §16 and makes the store
  validate rules the engine already knows. But with (a) the store would import the
  engine (cycle) or accept any status string, and a lost update between `cancel` and the
  engine loop is exactly the stale-state bug class the prototype had. Validating twice
  is cheap; the CHECK constraints are a third line against manual edits.
- **Decision:** (b). `task` holds the types, IDs (10-char Crockford base32, 50 random
  bits) and state machines. Every store transition is `UPDATE … WHERE status = from`;
  zero rows yields `ErrNotFound` or a `*ConflictError` with the actual state. Steps,
  attempts and sessions only change while their parent is in the right state; partial
  unique indexes allow one running attempt per step and one turn in flight per session.
  Process start tokens are stored as decimal text (uint64 does not fit SQLite INTEGER).
- **Review (independent agent):** found that `cancel` did not stop later side effects
  and that a resumed session could not be recorded. Fixed: attempts, sessions and turns
  start only while the task is active (running/reviewing); leaving the active states
  ends running attempts, interrupts turns in flight and resets running steps in the same
  transaction; `done` needs every step verified. A session row is one conversation: a
  process replaced within it updates the row (`SetSessionProcess`), a resume after a
  daemon restart is a new row with `resumed_from`, and the native id is unique among
  live rows only.
- **Consequences:** §16 lists `internal/task`. Tests `TestTransitionTaskConcurrent`
  (16 writers, one winner), `TestCancelRacesEngine`, `TestResumeAfterRestart`,
  `TestDoneRequiresFinishedWork`, `TestNativeIDUnique`.

## D-019 — Adapters report every process through `SessionSpec.OnSpawn`
*2026-11-25 · M3.5 prerequisite*

- **Context:** §5.1 requires each agent process to be recorded (PID + start time) before
  the first message, but the runtime contract hid processes inside the adapters. Claude
  also replaces its process within one conversation (stubborn interrupt, crash).
- **Options:** (a) expose the process on `Session` after `Open`; (b) a hook the adapter
  calls right after `procs.Start`, before any write.
- **Challenge (self-review):** (a) is simpler but too late: `Open` already wrote the
  handshake, so a daemon crash between start and record leaks an unrecorded process,
  and in-session respawns would go unnoticed. A hook runs on the adapter's goroutine and
  can block startup on a store write; that cost (one SQLite transaction) is the point.
- **Decision:** (b). `OnSpawn(ctx, procs.Identity) error` is called for every process
  start; an error wraps `runtime.ErrSpawnRejected`, kills the process and fails the
  operation; Claude does not retry its resume fallback after a rejection. The engine maps
  the first call to `store.RecordSession` and later ones to `store.SetSessionProcess`.
- **Consequences:** `runtimetest.SpawnRecorder` checks the ordering against each fake
  CLI; tests `TestOnSpawnRunsBeforeFirstWrite`, `TestOnSpawnRejectKillsProcess` per
  adapter, respawn coverage in `TestStubbornInterruptKillsAndResumes`.

## D-020 — M3 permission policy: file edits inside the worktree only
*2026-11-25 · M3.8*

- **Context:** `openchef run` drives real CLIs from M3 on, but the permission evaluator,
  shell parsing and approval queue (§9) are M4. Something must answer approval requests
  now.
- **Options:** (a) allow everything (`bypassPermissions`); (b) allow file changes inside
  the task's worktree, refuse commands and other tools; (c) wait for M4 before running
  real CLIs.
- **Challenge (self-review):** (b) blocks agents from running tests themselves, which
  lowers first-try quality. But (a) lets an injected prompt run arbitrary commands with
  the user's credentials, and (c) leaves M3 untestable end to end. OpenChef runs the
  verification command itself and feeds failures back, so (b) still closes the loop.
- **Decision:** (b): `acceptEdits` mode, invariant deny rules, an approver that allows a
  file change only when every path is inside the worktree and outside `.git`,
  `.openchef` and `.claude`, and denies commands/tools with an explanation; questions get
  "decide yourself and say so". Subscription pools lose API key variables (§13).
- **Consequences:** replaced by the M4 evaluator; until then agents cannot run tests.

## D-021 — Permission matching: parsed commands, opaque runners, redirections as file access
*2026-11-26 · M4.1/M4.2*

- **Context:** §9.2 requires Claude Code's rule syntax with shell commands parsed into
  simple commands. Several cases are not covered by Claude Code's documentation.
- **Decision:**
  - Commands are split with `mvdan.cc/sh/v3` (Bash dialect) across lists, pipes,
    subshells, command and process substitution, compound commands and `sh -c` strings
    (nested up to 4 levels). Every simple command must be allowed; any deny denies.
  - A command is *dynamic* when a word expands at run time (`$X`, `$(…)`, globs, `~user`,
    `$'…'`), when it has variable assignments (`PATH=… x`), when it defines a function
    (which can shadow any name), or when it runs code it receives (`eval`, `source`,
    `xargs`, `sudo`, `exec`, `ssh`, `find -exec` …). Dynamic commands are never allowed
    by allow rules; prefix deny rules still match their static leading words.
  - Wrappers (`env`, `timeout`, `nice`, `nohup`, `time`, `command` …) are unwrapped, so
    `timeout 5 rm -rf /` meets `Bash(rm:*)`.
  - Redirection targets are file access: `>`/`>>` are checked as Write, `<` as Read,
    with the workspace and protected-directory invariants; `~/` resolves to home.
  - Edit rules cover Edit/Write/MultiEdit/NotebookEdit; Read rules cover Read/Grep/Glob;
    a pattern without a slash matches at any depth; a directory pattern covers its
    contents.
  - A settings file with any invalid rule or unknown key fails to load.
- **Challenge (self-review):** stricter than Claude Code in places (assignments,
  functions), so some harmless commands ask. Asking costs a click; a bypass costs the
  user's machine. A fuzz test found a parser-induced panic (`()0`), fixed and kept as a
  regression; the evaluator also recovers from panics as "ask".
- **Consequences:** `TestBypassCorpus` (23 bypass attempts), `FuzzSplit`, mode table.

## D-022 — Approval queue: durable requests, integer ids, task events, restart expiry
*2026-11-26 · M4.3*

- **Context:** §9.2 sends `ask` decisions and agent questions to one queue answerable
  from the CLI, denied after 15 minutes. The queue must survive a daemon restart without
  answering on behalf of an agent that is no longer waiting.
- **Options:** (a) an in-memory queue; (b) a table (`approvals`) with compare-and-set
  decisions and an in-memory map of waiters; for viewers, (c) a separate
  `approval/requested` broadcast to every connection, or (d) task events on the existing
  watch stream.
- **Challenge (self-review):** (a) loses the audit trail and leaves `openchef approvals`
  empty after a restart. A restarted daemon cannot hand an old answer to the agent: its
  process was killed and the resumed turn asks again, so keeping the old row pending
  would invite a decision that reaches no one. (c) pushes notifications to clients that
  never read them (back-pressure on the CLI) and duplicates (d), which `run`, `logs -f`
  and the future TUI already consume.
- **Decision:** (b) + (d). Ids are integers (`openchef approve 3`). `Ask` blocks until
  the user decides, the request expires (15 min → deny; a question gets "decide yourself
  and say so"), the agent stops waiting (`withdrawn`) or the daemon stops (left pending).
  On start, pending rows are settled `expired` by the daemon and stay listed. Requests
  and decisions are `approval_requested` / `approval_decided` task events; `--quiet`
  still shows requests. Summaries are stored and shown whole, multi-line commands
  included: people approve exactly what runs. An unwatched request raises a desktop
  notification (osascript / notify-send, arguments never through a shell; none on
  Windows). Until M4.4 wires the evaluator, file changes are still decided by the
  worktree rule of D-020 and everything else goes to the queue.
- **Consequences:** `TestApprovalRoundTrip`, `TestDenyAndAnswer` (through the daemon and
  CLI), `TestAskTimesOut`, `TestAskWithdrawnWhenAgentStops`,
  `TestRestartExpiresPendingRequests`, `TestDecideApprovalRace`. D-020's "no shell" now
  means "no shell without the user's approval".

## D-023 — Per-task permission mode; evaluator before the queue; links judged where they lead
*2026-11-26 · M4.4*

- **Context:** §9.3 maps six modes onto three runtimes; D-020's fixed policy
  (acceptEdits, worktree-only edits) and the M4.1 evaluator had to meet. The evaluator
  also compared paths lexically.
- **Options:** mode per daemon, per repository (settings only) or per task; for links,
  lexical matching, fully resolved matching, or resolved matching only for paths that
  leave the workspace through a link inside it.
- **Challenge (self-review):** a per-repository mode cannot express "this one task in
  plan mode". Fully resolved matching breaks users' absolute rules on macOS
  (`/etc` → `/private/etc`, `/var` → `/private/var`); lexical matching lets
  `src/out -> /` turn `Read(src/**)` into a read of anything. A bypass that a settings
  file can switch on for every task is one committed line away from running every
  command unasked.
- **Decision:** the mode is stored on the task (`permission_mode`, migration 0004; older
  rows are `acceptEdits`, the policy they ran under), chosen by `run --permission-mode`
  or the merged settings' `defaultMode`, else `manual`. `bypassPermissions` is accepted
  only when asked for per task and only in an isolated worktree; as a default it is
  refused. Each session loads the settings again and builds an evaluator for its
  worktree; private paths are OpenChef's home except `worktrees/`. The approver asks the
  evaluator first: allow and deny are answered at once, ask goes to the queue (D-022);
  mode changes (ExitPlanMode) always go to the user. Deny and ask rules are also passed
  natively (Claude), after the invariants. Paths that leave the workspace through a link
  inside it are judged where they lead; deny rules match both spellings; write
  invariants use pathguard (case, Windows aliases).
- **Consequences:** D-020's approver is gone. `TestApproverModeMatrix` (every mode ×
  Claude/Codex/agy request shapes, out-of-workspace writes denied in all),
  `TestSymlinkedPaths`, `TestSessionPolicy`, `TestRunPermissionMode`,
  `TestPermissionModeMigration`, `TestPermissionModesMatchRuntime`. Known limit: an
  absolute deny rule under a system link is matched as spelled and as resolved, not
  through every alias of its ancestors.

## D-024 — ask_user: a hidden `openchef mcp ask-user` per session, one tool, long timeout
*2026-11-26 · M4.5*

- **Context:** Claude's own `AskUserQuestion` is disabled (D-006), so questions need
  OpenChef's MCP tool; Codex and agy accept the same server. A question can wait up to
  the queue's 15 minutes, and the CLIs give MCP tool calls about a minute by default.
- **Options:** (a) an MCP server inside the daemon on a second socket; (b) this
  executable's hidden `mcp ask-user --task --agent` on stdio, forwarding to the daemon
  over the existing API (`approval/ask`).
- **Challenge (self-review):** (a) adds a listener and a second protocol to the daemon
  and needs a way to tell which agent is asking. (b) is a process per session, but CLIs
  start MCP servers that way anyway, the task and agent are fixed by its arguments, and
  each question dials the daemon afresh, so a daemon restart between questions does not
  break it. The socket is the user's own (0600), so any local process of the user could
  already queue a question; nothing new is exposed.
- **Decision:** (b). The server lists only `ask_user` (every listed tool is offered to
  the model, M1 findings); questions over 4000 characters are refused; an unanswered or
  unreachable question returns "decide yourself and say so" as a normal result. The
  call is withdrawn when the MCP connection closes. `MCPServer.ToolTimeout` (queue
  timeout + 1 min) becomes `MCP_TOOL_TIMEOUT` for Claude and `tool_timeout_sec` for
  Codex; agy has no known setting.
- **Consequences:** `TestAskUserThroughTheDaemon` (MCP client → server → daemon queue →
  `openchef answer` → tool result), `TestHandshakeAndToolList`, `TestAskUserAnswers`,
  `TestAskUserUnanswered`, adapter timeout tests.

## D-025 — Quota wiring: stored observations, stream before probes, kill switch in the store
*2026-11-26 · M4.6*

- **Context:** the quota package (#41) merges observations and runs probes; the daemon
  must feed it, keep it across restarts, schedule probes and let the user stop them.
- **Decision:**
  - Every observation (L1 from agent streams through an engine-sink decorator, L2 from
    probes) goes to `quota_observations`; a restarted daemon replays the last 8 days (a
    weekly window) and prunes older rows. Storing is asynchronous and bounded: the
    engine's sink must not block, so a backlog keeps the observation for routing and
    logs that it was not stored.
  - Probes run every 5 minutes, but a pool observed within that interval is skipped:
    the agents' own streams (Codex's rate-limit methods, Claude's `rate_limit_event`)
    are preferred over undocumented endpoints (§10.2). `openchef quota --refresh`
    always probes, still at most every 30 s per probe.
  - Switches: `quota.probes` and `quota.providers.<provider>` in `config.toml` decide
    which probes exist; `openchef quota probes off|on` is stored in the state database
    (`flags`) so it outlives the daemon without OpenChef rewriting the user's file. The
    config wins: the CLI switch cannot turn on a probe the config left out.
  - `openchef init` explains the probes and asks on a terminal (default yes), takes
    `--quota-probes on|off`, and writes the choice into the config. `doctor` shows a
    running daemon's probes, including one that disabled itself after a shape change.
  - L3 (per-step cost history) moves to M4.7, where admission is its only consumer.
- **Consequences:** `TestQuotaObservePersistAndRestore`, `TestQuotaProbesAndKillSwitch`,
  `TestQuotaShapeChangeDisablesProbe`, `TestQuotaScheduleSkipsFreshPools`,
  `TestQuotaSinkRecordsStreamSignals`, `TestProbesFor`, `TestQuotaCommand`,
  `TestDoctorShowsDisabledProbes`, `TestInitRecordsProbeConsent`.

## D-026 — Admission by measured window share; budgets as windows; quota router replaces cooldowns
*2026-11-26 · M4.7*

- **Context:** §10.3 admits a step to a pool only if its headroom covers the step's
  estimated cost with a 30 % margin, red pools take small steps only, and API pools have
  USD budgets. Providers report windows as percentages, not tokens, so a token-based
  cost estimate cannot be compared with headroom.
- **Decision:**
  - L3 measures each attempt's cost in the unit headroom is in: the largest increase of
    a window's utilization between the attempt's start and end (`quota.Spent`), skipped
    when nothing newer was observed or the window reset meanwhile; stored in `usage`.
    The estimate is the 75th percentile of the pool's last 20 costs; under three samples
    it is unknown and the step is admitted.
  - `quota.Admit`: exhausted → never; red → only estimates ≤ 5 % of a window;
    otherwise estimate × 1.3 ≤ the tightest window's headroom.
  - API pools' reported turn costs (`total_cost_usd`) are stored in `spend` and become
    windows `daily_usd` / `monthly_usd` (utilization = spent / budget, reset at local
    midnight / the first of the month), so health, admission and routing treat budgets
    exactly like quota windows. Subscriptions' notional costs are not recorded.
  - The app's quota router builds router candidates (tier, health, headroom, reset,
    API, weights, admission) and lets the phase's strategy choose; a pin skips
    selection but not admission; with no candidate it waits for the earliest reset.
    Effort is resolved for the chosen pool (profile, plan tier, pressure, surplus).
    A quota failure on a turn records the pool as exhausted until its reset (an hour if
    unknown). The M3 in-memory cooldowns are gone.
- **Consequences:** `TestRoutingFollowsQuota`, `TestPinnedAgentIsAdmittedNotChosen`,
  `TestWeightedSharesAndEffort` (3:1 within ±1 over 40 steps), `TestSpent`,
  `TestEstimate`, `TestAdmit`, `TestQuotaStepCosts`, `TestQuotaBudgets`. Weighted
  counters live for the daemon's life; persisting them is left for when a TUI shows
  shares.

---

## D-027 — Rewrite in Rust before R5; the Go code becomes the reference
*2026-11-27 · maintainer decision*

- **Context:** M0–M4 were built in Go. A proposal (`docs/research/2026-11-27-rust-proposal.md`)
  argued for Rust. The maintainer ruled out cost as a factor and chose to build v1 by
  AI-assisted ("vibe") coding with a three-person team, asking which language gives the
  most stable product.
- **Options:** keep Go; Rust; TypeScript (Node/Bun).
- **Challenge:** "Rust does not catch the bugs Go's stress runs found": true for timing
  and logic races, deadlocks and CLI quirks, which stay test-only in both languages.
  "Go can sandbox too": true (runc, gVisor, Lima, `go-landlock`), so the sandbox does
  *not* require Rust. "brush-parser is younger than mvdan/sh": true; handled by spike S4
  and fail-closed parsing. TypeScript was rejected because process-tree control and
  Linux confinement need native addons or external helpers, the failure class that
  ended the Python prototype.
- **Decision:** Rust. What decides it is compile-time correctness for AI-written code
  whose reviewers do not read every line: exhaustive `match` over state machines and
  protocol enums, no null, mandatory `Result` handling, no data races, panics isolated
  per task, and direct `pre_exec` confinement. Go's advantage in cancellation semantics
  is neutralised by the actor rule (D-030).
- **Consequences:** new repository `ugurboz/openchef`; the Go repository is renamed
  `ugurboz/openchef-go`, tagged `go-reference` and archived after M4 is merged there.
  No new features in Go. Research notes and `testdata/` carry over unchanged; each
  ported crate brings its Go tests over first (§18).

## D-028 — No container runtime: a built-in OS sandbox replaces the Docker backend
*2026-11-27 · maintainer decision*

- **Context:** v0.6 §21.3 ran sandboxed tasks in Docker-compatible containers. The
  maintainer requires OpenChef to work with no external runtime on any platform.
- **Options:** container backend (as before); built-in OS sandbox; built-in micro-VM.
- **Challenge:** "an OS sandbox is weaker than a VM": yes — it confines files and
  process privileges, not the kernel or network. For v1 the threat is an agent writing
  outside its worktree or reading private files; the kernel sandbox stops both. A VM
  per task means shipping kernels, images and a vsock protocol on three hypervisors, a
  product of its own; it is planned after v1 (§22). "agent CLIs may not run confined"
  (Keychain, nested sandboxes): unknown, hence spikes S1–S3 before any design is fixed,
  and fail closed when confinement is impossible.
- **Decision:** §21.3 is the built-in OS sandbox (Seatbelt, Landlock + seccomp, Windows
  restricted token or AppContainer), applied to agent processes and verification
  commands, with one policy intent on all OSes, failing closed. The container backend,
  `openchef export` and the container spikes are dropped.
- **Consequences:** `openchef-sandbox` crate (R4) after spikes S1–S3 (R1);
  `bypassPermissions` unattended requires `--sandbox`; network stays open in v1.

## D-029 — Bounded cross-provider debate is part of v1
*2026-11-27 · maintainer decision*

- **Context:** v0.6 §7.5 stored review findings without fixing them. The maintainer
  named "agents that debate and converge overnight" the product's core difference.
- **Options:** keep passive review; unbounded multi-agent chat; a bounded state machine.
- **Challenge:** "debate burns the subscriptions most users have (Pro/Plus)": hence
  severity gating (only located blockers/majors cost a turn), round limits by
  importance, a per-task turn budget, admission for every debate turn and depth lowered
  under quota pressure. "agents talk each other into wrong answers": verification stays
  the only judge of success; rebuttals go to a third provider or become `disputed`.
- **Decision:** §7.5 as amended: plan council for `high`/`critical`, review → fix rounds
  per step by importance, integration audit for `high`/`critical`, stagnation, quota and
  budget breakers.
- **Consequences:** findings schema in `openchef-core`; debate in `openchef-engine`
  (R5); the TUI gets a debate view (R6); the report lists disputed and open findings.

## D-030 — Rules for an AI-written Rust codebase
*2026-11-27 · R0*

- **Context:** all code is written by AI agents driven by three maintainers who do not
  review every line; the lead reviewer is also an agent. The rules must be enforced by
  tools, not by memory.
- **Decision:**
  - **Actors, not shared locks.** Every stateful component (session, task runner,
    store writer, approval queue, quota tracker, daemon core) is one `tokio` task that
    owns its state and receives typed command enums over bounded channels, with
    `oneshot` replies. No `Arc<Mutex<…>>` graphs; a `Mutex` is allowed only for a
    leaf value never held across `.await`.
  - **Cancellation is a message.** Long operations take a `CancellationToken`; nothing
    relies on dropping a future to stop a protocol exchange.
  - **Closed enums** for runtimes, events, failure kinds, statuses and findings;
    `#[non_exhaustive]` only on types crossing the API boundary.
  - **Workspace lints** deny `unsafe_code` (allowed in `openchef-procs` and
    `openchef-sandbox` only), `unwrap_used`, `expect_used`, `panic`, `todo`,
    `unimplemented`, `dbg_macro`, unused results; tests may `unwrap`.
  - **Errors** are `thiserror` enums per crate; `anyhow` only in the binary.
  - **Raw first:** every line from a CLI is written to `raw.jsonl` before it is parsed.
  - **Ported tests first** (§18); **one issue per PR**; the task card's file scope is a
    boundary (touching other crates needs the lead's approval on the issue).
  - **Review:** CI green on three OSes (and stress where it applies) and the lead's
    review are required to merge; the lead merges.
- **Consequences:** `AGENTS.md` (read by Claude Code through `CLAUDE.md`, by Codex and
  agy directly) carries these rules into every contributor's agent session.

## D-031 — Shell parsing: brush-parser behind a length and nesting guard
*2026-11-27 · R1.4 (spike S4)*

- **Context:** §9.2 needs a real shell parser in Rust. Go used `mvdan.cc/sh`.
- **Options:** `brush-parser`; tree-sitter-bash; a hand-written splitter.
- **Challenge:** "a younger parser will miss constructs": measured instead of assumed —
  the Go splitter itself was run as an oracle on 147 lines (the Go corpora plus 85
  hostile cases); the Rust port matches on 142, and every difference fails closed or is
  a Go bug (`> file` was not reported as a write). The oracle also caught a port bug
  that hid `export X=$(curl …)`. "It can crash": yes — deep nesting overflows its stack
  and aborts the process, which no `catch_unwind` stops.
- **Decision:** `brush-parser`, pinned exactly; reject lines over 64 KiB or nested deeper
  than 64 before parsing (safe on a 2 MiB stack even in debug builds); parse on a
  dedicated 8 MiB-stack thread; fail closed on parse errors, NUL bytes, `select` and
  substitutions inside parameter expansions; keep the corpus and oracle output as a CI
  test so a parser upgrade must pass it.
- **Consequences:** R4.1 starts from `spikes/shell-parser`.

## D-032 — Built-in sandbox: one sandbox, private TMPDIR, per-repository caches
*2026-11-27 · R1.1, R1.2 (spikes S1, S2)*

- **Context:** S1 (macOS Seatbelt, real logged-in CLIs) and S2 (Linux Landlock +
  seccomp on three Ubuntu kernels) tested the §21.3 policy.
- **Findings that decide the design:** the kernel stopped an agent with every permission
  prompt off from reading a secret or writing outside the worktree, on both OSes;
  sandboxes do not nest (Codex's own sandbox fails inside ours; agy then retries
  unsandboxed); unprivileged user namespaces are blocked on Ubuntu 24.04; compilers fail
  without a writable `TMPDIR`; Go needs redirected caches; DNS needs the resolver's
  symlink target.
- **Challenge:** "redirected caches make every task re-download": per-repository, not
  per-task, caches keep repeat runs fast; sharing the user's global caches would let an
  agent poison them for every other project, so that stays opt-in.
- **Decision:** §21.3 as amended — OpenChef's sandbox replaces the CLI's own (Codex
  `danger-full-access` under it), private `TMPDIR` always, per-repository caches,
  canonicalized paths, Linux through a self-confining helper without namespaces,
  macOS through generated Seatbelt profiles. Windows waits for S3.
- **Consequences:** D-003 (Codex sandbox per mode) and D-015 (agy sandbox always on) are
  amended in R4.5/R4.6 for sandboxed runs; `openchef doctor` probes that each sandbox
  applies.


## D-033 — Interim approver until approvals: mode-aware, no commands
*2026-11-28 · R3.6 · superseded by D-034*

- **Context:** R3 runs real agents end to end, but the evaluator, approval queue and
  `ask_user` arrive with R4. Go M3 forced `acceptEdits` on every session; the Rust
  service already stores a permission mode per task (`--permission-mode`), so the
  interim policy must not contradict it.
- **Decision:** the daemon's approver allows file changes only inside the task's
  worktree and outside `.git`, `.openchef` and `.claude` (symlinks resolved, Windows
  aliases refused: `openchef-permissions::pathguard`), refuses every change in `plan`,
  refuses shell commands and other tools in every mode except `bypassPermissions`, and
  refuses mode changes. Questions get a "make the reasonable choice and say so" answer.
  `bypassPermissions` sessions open only in an isolated worktree. The session's mode is
  the task's mode, so the adapters' native mapping (D-008, D-003, D-015) applies.
- **Challenge:** "refusing commands makes agents useless": OpenChef runs the step's
  verification itself, so a step completes without the agent running commands, as in
  Go M3; users who want commands ask for `bypassPermissions` per task.
- **Consequences:** R4.2 replaces the approver with the evaluator and the queue; the
  path guard stays as the invariant check under it.

## D-034 — Evaluator-backed approver: per-session policy, the queue for the rest
*2026-11-28 · R4.2 · supersedes D-033*

- **Context:** with the evaluator (R4.1) and the approval queue (D-022) in place, the
  interim approver of D-033 goes. Go `internal/app/{approver,policy,notify}.go` is the
  reference.
- **Decision:** each session loads the user, project and local settings files again
  (edits apply to the next session; a broken rule fails the session instead of
  silently allowing). The task's mode, fixed when it was created, wins over
  `defaultMode`; `defaultMode` sets new tasks' mode but can never be
  `bypassPermissions`, which runs only in an isolated worktree. The evaluator allows or
  denies at once ("OpenChef refused this: <reason> (<rule>)."); `ask` verdicts, every
  mode change and every question go to the queue, with the normalized request and the
  evaluator's reason kept as audit JSON. Deny and ask rules also reach the CLIs
  natively, after the invariant deny rules. Private, for reads and writes: every file
  of the OpenChef home except the worktrees, including the SQLite `-wal`/`-shm`/
  `-journal` companions of `state.db` (Go missed these). A queue failure while the
  agent waits is a denial the agent can read; an agent that stopped waiting or a
  stopping daemon is an approver error, which the adapter turns into a denial. The
  desktop notifier runs `osascript` / `notify-send` with title and body as separate
  arguments, on its own thread, killed after 5 s.
- **Challenge:** "asking for every edit in manual mode makes `openchef run` stall":
  that is what manual means in Claude Code too; `acceptEdits` or a `defaultMode` in the
  settings is the one-line answer, and `--quiet` runs still show the request with the
  commands that settle it.
- **Consequences:** the path guard remains the invariant check inside the evaluator.
  `ask_user` over MCP (R4.2b) reuses the same queue.

## D-035 — Quota in Rust: two actors, a published view, budgets in the local day
*2026-11-28 · R4.3, R4.4 · ports D-025, D-026*

- **Context:** Go's `service.Quota` was a mutex-guarded object whose probes ran under
  the same lock holder; D-030 asks for actors, and the engine's `Router::route` is
  synchronous.
- **Options:** (a) one actor owning tracker, costs and probes; (b) a monitor actor for
  state and a probe-runner actor for the L2 probes, with the monitor publishing an
  immutable view over a `watch` channel; (c) a lock around the tracker.
- **Challenge:** (a) blocks every observation, report and routing read for up to 10 s
  per probe. (c) is the shared-lock graph D-030 forbids. A `watch` view is copied on
  every change, but a tracker holds a handful of pools, so the copy is cheap next to
  a turn.
- **Decision:** (b). Engine-facing calls (`observe`, `attempt_*`, `spend`, `exhausted`)
  use `try_send` into bounded queues and never block; a full queue loses the record
  and logs it (Go kept such observations in memory; here the monitor's queue is the
  memory, 1024 deep). A manual refresh asks the runner, which hands observations to the
  monitor before replying, so the report that follows includes them. The router
  reads the latest view synchronously. Quota sees engine events through a new
  service tap (`Deps.tap`), ahead of the event log. Windows store `duration` as whole
  seconds. API budgets' days and months use the local UTC offset read by `openchef
  daemon run` before its runtime starts threads (Unix cannot read it safely later);
  unknown means UTC. The `ask_user` question limit counts characters, not bytes.
- **Consequences:** `Ordered` and its cooldowns stay in the engine for tests; the
  daemon routes with `QuotaRouter`. Tests port `TestQuota*`, `TestProbesFor`,
  `TestQuotaSinkRecordsStreamSignals`, `TestRoutingFollowsQuota`,
  `TestPinnedAgentIsAdmittedNotChosen`, `TestWeightedSharesAndEffort` and
  `TestQuotaCommand`. `openchef doctor` does not show a running daemon's probes yet
  (#43).
