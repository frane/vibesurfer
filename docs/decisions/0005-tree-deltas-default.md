# 0005 — Tree deltas are the default

**Status:** Accepted (M0).
**Date:** 2026-05-06.

## Context

A modern web page's a11y tree is on the order of hundreds to low
thousands of nodes. An agent loop that reads the tree on every step,
acts, then reads again, will pay for the entire tree N times. For
moderately complex apps, that is the dominant token cost in the loop.

Most reads in an agent loop change very little — a value updated, a
class toggled, a button enabled. Re-emitting the entire tree on every
read is the obvious thing to do and the wrong thing to do.

## Decision

**`vs_view` returns deltas by default.** A full tree is emitted only
when:

- It is the agent's first `vs_view` on a page.
- The page navigated (`? nav <url>`).
- The viewport changed (`? viewport_changed <W>x<H>`).
- An auth blob was loaded (`? auth_loaded <name>`).
- The agent explicitly passes `--full`.

Otherwise, the response body is a sequence of delta operations:
`+` (add), `-` (remove), `~` (update), `>` (move), `*` (replace).

When a subtree's edit distance exceeds 40% of nodes (size > 5), a
single `*<ref>` is preferred over many fine-grained ops.

## Consequences

- The daemon caches the last-emitted tree per `(page_id, agent_id)`.
  On daemon restart this is reconstructed from `refs` in SQLite.
- Diff computation is on the daemon. Start with a top-down recursive
  diff; bail to `*<ref>` on the 40% threshold. Optimize only if
  profiling shows it matters.
- "Nothing changed since last token" reads collapse to `@<token>` and
  a blank line. Same token = no work for the agent.
- Adding stale-state semantics on top is straightforward: if the
  agent's last-seen token is unknown to the daemon (restart, new
  agent identity), the daemon emits a full tree.

## Rejected

- "Full re-dumps; let agents diff client-side." Pushes the cost
  back onto the loop. The agent pays in tokens; the daemon pays in
  CPU. The latter is cheaper.
- "Only emit deltas when the agent opts in." Wrong default. The
  expensive thing should be opt-in, not opt-out.
- "Tree-edit distance via Zhang-Shasha." Overkill for the depth and
  branching factor of typical a11y trees. The recursive top-down diff
  is good enough; revisit if profiling proves otherwise.
