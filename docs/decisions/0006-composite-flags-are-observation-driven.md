# 0006 — Composite flags are observation-driven

**Status**: Accepted (M5.5).
**Context**: M5.5 introduces opt-in flags that bundle a follow-up read into the response of a mutating or navigating primitive — `vs_open --view`, `vs_wait --view`, `vs_act --view`, `vs_view --layout=N,M`, `vs_view --read=N`. Each one collapses a canonical two-call sequence the agent does on every turn into a single call. They are token- and round-trip-cheap because the second response body just rides along on the first.

## Decision

Composite flags are added when an agent has been observed doing the two-call pattern repeatedly in real use — in demo scripts, production logs, or e2e tests. They are **not** added speculatively. Multi-call patterns that haven't appeared in usage evidence don't get a flag, no matter how plausible they sound.

## Consequence

- The five M5.5 flags above each correspond to a documented pattern in our existing demo scripts and integration tests.
- Reviewers should reject proposed composite flags whose justification is "the agent might want…". The proposal must cite a script or log line where the two-call sequence is happening.
- Pruning works in reverse: if a composite flag's two-call origin disappears from observed usage, the flag is a candidate for deprecation in a later milestone.
- The trap we are explicitly avoiding: an `--include-everything` mode where every primitive bundles every conceivable follow-up. That branches the response shape, complicates parsing, and makes the wire unbounded.

## Out of scope

- Multi-page batch (`vs view PAGE_A,PAGE_B`). No observed need; agents alternate, they don't need simultaneous state.
- Bulk act (`vs act 1 click ; vs act 2 fill ...`). The second action's token is invalidated by the first; no round-trip savings.
- `--include-screenshots` or any other ride-along that isn't on the M5.5 list.

## See also

- [`0007-pipeline-syntax-v2.md`](./0007-pipeline-syntax-v2.md) — the longer-term direction is explicit pipeline syntax, of which composite flags are a foreshadow.
