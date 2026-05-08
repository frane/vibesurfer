# 0004 — Stable refs, not line numbers

**Status:** Accepted (M0).
**Date:** 2026-05-06.

## Context

agented uses 1-indexed line numbers because a file is a 1D array of
lines. A line number is a stable, human-meaningful, position-derived
identifier. If a file edit shifts line 42 to line 47, that is fine —
the tool reports the new number, the agent updates its model.

The DOM is not a 1D array. It is a tree that mutates from JS, network
events, and timers, often without any visual change. There is no
position-derived identifier that survives mutation:

- DOM order changes when a list reorders.
- CSS selectors break when classes change between deploys.
- XPath expressions are brittle and verbose.
- Indented "line numbers" within a serialized tree are an accident of
  serialization, not a property of the element.

## Decision

**Elements are addressed by integer refs allocated by the daemon.**
Refs are stable for the lifetime of a session and never reused. The
agent receives them in `vs_view` and uses them in `vs_act`, `vs_read`,
`vs_mark`, `vs_layout`, `vs_capture`.

## Consequences

- The daemon must track ref → element identity across snapshots. The
  matcher uses `(role, parent_path, content_hash)` heuristics; see
  M4 for the algorithm.
- A removed-then-readded element gets a new ref, even if its DOM path
  is identical. Refs encode identity-since-last-seen; they do not
  encode current DOM position.
- Refs reset on top-level navigation. The daemon emits `? nav <url>`
  before a fresh full tree.
- Marks (`vs_mark <ref> <name>`) are the persistent counterpart: a
  named anchor stored by DOM path, recoverable across mutations.

## Rejected

- "Line numbers within the serialized tree." Coupling identity to a
  specific serialization is what keeps Playwright-style scripts so
  fragile. We are not repeating the mistake.
- "CSS selectors as primary identity." Rotates with deploys, ambiguous
  under shadow DOM, hard to bound to a single element.
- "Stable string ids derived from content hashes." Useful as a *part*
  of the matching heuristic, not as the agent-visible identity. The
  agent should pass an opaque integer, not reason about hashes.
