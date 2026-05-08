# 0002 — No transactions

**Status:** Accepted (M0).
**Date:** 2026-05-06.

## Context

The agented project, whose protocol shape vibesurfer borrows, has
transactions — `ae begin`, `ae commit`, `ae rollback` — because file
edits are atomic and reversible. The natural question for vibesurfer is
whether browser actions should also live inside transactions.

## Decision

**No transactions.** Browser actions hit remote systems. Once a click
sends a payment, the payment is sent — there is no rollback. Modeling
side-effecting actions as if they were rollbackable is, at best, a
useful fiction; at worst, a footgun that lulls agents into believing
they can undo what they cannot.

## Consequences

- The wire protocol has no `vs_begin` / `vs_commit` / `vs_rollback`.
- Atomicity across multiple primitives is not provided. If an agent
  needs "do A then B atomically," that is the agent's problem, not the
  protocol's.
- Freshness — the property transactions sometimes provide as a side
  effect — is solved separately by `state_token`. Each write either
  commits at the agent's intended pre-image or fails loudly.
- The audit log records every primitive call regardless. Reconstructing
  a logical group is done with `--group=<label>` and `vs_log
  --group=...`, not with transactions.

## Rejected

- "Optimistic transactions that batch reads and verify on commit." Adds
  complexity, doesn't actually buy anything we don't already get from
  per-write `state_token` checks.
- "Transactions for read-only sequences (snapshots)." A read-only group
  is just a read followed by reads; the token semantics already cover
  this.
