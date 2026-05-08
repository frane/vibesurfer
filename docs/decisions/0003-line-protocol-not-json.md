# 0003 — Line-oriented protocol, not JSON

**Status:** Accepted (M0).
**Date:** 2026-05-06.

## Context

The default modern wire format is JSON. It is human-readable, ubiquitous,
and supported by every language. It is also expensive in tokens — every
field name appears in every record, every value is wrapped in quotes,
every record is wrapped in braces. For a protocol whose hot path is "an
agent reads a tree on every step," JSON is the wrong default.

## Decision

**The wire format is line-oriented, tab-separated where multiple fields
appear, indented for tree structure, and uses short codes for repeated
concepts (roles, ops, error codes).** No JSON on the hot path.

## Consequences

- The format is the cost model. Reducing tokens is reducing the wire.
- `serde_json` and friends are not on the hot path; we ship a tiny
  parser/encoder in `vs-protocol` instead.
- Config files (TOML/YAML) and the `--json` debug output remain JSON-
  capable. The contract for agents is the line format.
- The format is harder for humans to skim than JSON. We accept this;
  the primary consumer is an agent, the secondary is `vs --json`.

## Rejected

- "Use JSON for v1 and optimize later." The cost model would be wrong
  for the entire v1 lifecycle. Agents trained against JSON shapes would
  resist a later switch.
- "Use protobuf / msgpack / ...". Optimizes bytes, not tokens. Agents
  pay tokens, not bytes; binary wire formats are also harder to debug
  by eye.
- "JSON Lines." Closer, still pays the field-name tax on every record.
