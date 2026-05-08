# vibesurfer

A headless browser engineered for AI agents. Stateless CLI, persistent
daemon (`vibesurferd`) wrapping WebKit (WPE on Linux, the system framework
on macOS), and a line-oriented wire protocol that treats token cost,
freshness, and governance as first-class concerns.

> **Status:** pre-alpha. Milestone M0 (repo skeleton + CI) only. Nothing
> renders yet. See [`docs/ROADMAP.md`](docs/ROADMAP.md) for the build order.

## Why this exists

The agent-control surfaces in use today (CDP, harness layers on top of CDP,
WebMCP) were designed for humans attaching DevTools to a running Chrome, or
for sites that opt in to declaring tools. Neither solves the problems an
autonomous agent actually has: stale state, ballooning token cost on
re-reads, no audit trail, no governance.

vibesurfer's wedge is the **protocol**, not the engine. The shape borrows
from [agented](https://github.com/frane/agentd) — stateless CLI, SQLite
state, sync RPC, optimistic concurrency via `state_token`, line-oriented
output — adapted for a tree that mutates from JS, network, and timers
rather than a flat array of lines.

## Documentation

- [`docs/PROTOCOL.md`](docs/PROTOCOL.md) — wire protocol spec (single source
  of truth)
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — daemon, CLI, SQLite,
  engine boundary
- [`docs/PRIMITIVES.md`](docs/PRIMITIVES.md) — the 19 primitives, one
  section each
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — milestones M0..M6 with exit
  criteria
- [`docs/SKILL.md`](docs/SKILL.md) — agent bootstrap (placeholder until M6)
- [`docs/codes.md`](docs/codes.md) — role codes, viewport presets, error
  and warning codes
- [`docs/decisions/`](docs/decisions/) — architectural decision records

## Repository layout

```
crates/
  vs-cli/          # `vs` binary, stateless
  vs-daemon/       # `vibesurferd`, long-running, owns the engine
  vs-protocol/     # wire format encoder/decoder, shared types
  vs-store/        # SQLite schema and queries
  vs-engine-webkit/ # WebKit FFI (WPE on Linux, system framework on macOS); feature-gated
docs/              # spec, architecture, ADRs
fixtures/          # static HTML for integration tests
tests/             # workspace-level integration tests
skills/            # composable agent skills
.github/           # CI workflows
```

## License

[MIT](LICENSE).
