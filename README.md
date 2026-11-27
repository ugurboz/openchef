# OpenChef

A local orchestrator for AI coding agents. OpenChef drives the agent CLIs you already
use — Claude Code, Codex and Antigravity — through one workflow: plan, approve,
implement in isolated git worktrees, verify with real commands, and have agents from
different providers review and fix each other's work. When a subscription runs out of
quota, work moves to another one; when all are exhausted, it waits and resumes.

**Status:** being rewritten in Rust (v1.0-rust). The earlier Go implementation is
archived at `ugurboz/openchef-go` (tag `go-reference`) and serves as the behavioural
reference.

- Architecture: [`docs/architecture.md`](docs/architecture.md)
- Decisions: [`docs/decisions/log.md`](docs/decisions/log.md)
- Roadmap and task cards: [`docs/plan/`](docs/plan/roadmap.md)
- Contributing (humans and agents): [`AGENTS.md`](AGENTS.md), [`docs/plan/team.md`](docs/plan/team.md)

Licensed under Apache-2.0.
