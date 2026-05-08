# 0007 — v2 wire pipeline syntax (forward-compatible groundwork only)

**Status**: Accepted in principle (M5.5). **Not shipped on the wire** in M5.5; daemon-side groundwork is in place.
**Context**: A v2 wire format will accept multiple primitives per request frame, separated by `|`. Example: `vs_open URL | vs_view | vs_layout 1,2,3`. The agent sends one frame, gets one composite response, saves a round trip versus the equivalent three-frame sequence.

## Decision

- The daemon's request-handling has been refactored to accept a `Vec<Primitive>` as of M5.5 PR1. `Daemon::dispatch(Vec<Primitive>) -> Vec<DispatchOutcome>` is the single entry point for both the wire path and in-process tests. Every primitive in the vec gets its own audit row, its own state-token validation, its own envelope. Per-primitive errors do not abort the rest of the sequence.
- Composite flags introduced in M5.5 (`--view`, `--layout=`, `--read=`) are implemented internally by the dispatcher expanding to a multi-primitive `Vec<Primitive>` against this same entry point. The wire still delivers one primitive per frame.
- The wire syntax change to accept `|`-separated multi-primitive frames is therefore a **parser-only** change in a later milestone. No further dispatcher rewrite is required.

## Consequence

- We pay the M5.5 dispatch refactor once and amortize it across both the composite flags and the eventual v2 wire syntax.
- Composite flags in M5.5 will continue to work after v2 ships and may eventually be deprecated in favor of explicit pipelines once adoption is verified. Until then, both styles work side by side.
- The response shape on the wire stays one envelope+body per primitive, terminated by a blank line. v2 just sends N of these in a single frame's response. Existing parsers handle this without changes.
- We do not commit to a specific v2 ship date in this ADR.

## Open questions (deferred)

- Should `|`-separated frames be transactional (all-or-nothing)? No: per-primitive errors are inline, sequence does not abort. Agent decides what to do.
- How does the v2 parser handle the trailing blank line within a multi-response? Same as today: each outcome is followed by a blank line; the frame is complete when N outcomes have been written.
- Cancellation mid-pipeline? Out of scope until we have a use case.
