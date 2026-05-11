---
name: debug-failed-action
version: 0.0.1
description: When your last `vs_act` returned an unexpected result (NOT_FOUND, no visible state change, suspected silent failure), run this skill with the action's pre-token. Pulls console errors, network failures, and current performance into one report.
---

# debug-failed-action

## When to use

You ran `vs_act` and one of:

- The next `vs_view` looks identical to the previous one (no DOM change).
- You got `! NOT_FOUND ref=N` even though the ref was in the most recent tree.
- A button you clicked appears `disabled` / `aria-busy` / `pointer-events: none` in the DOM but not in the semantic tree.
- A form submission completed in the UI but the underlying API call returned 4xx/5xx silently.

## What it does

Runs three on-demand inspections in sequence against the page that owns the failed action:

1. `vs_inspect console --since=<token> --level=error,warn` — every console.error / console.warn / uncaught exception captured since the action's pre-token.
2. `vs_inspect network --since=<token> --status=4xx,5xx` — every 4xx/5xx network response since the action's pre-token.
3. `vs_inspect performance` — Web Vitals + heap + DOM-stat block.

The output is concatenated and sectioned by source, prefixed with one-line headers.

## Arguments

- `token` — the `state_token` you held *before* issuing `vs_act`. The skill uses it as `--since=<token>` for both console and network slices.
- `page` — the `p_<id>` from `vs_status`. If omitted, the skill uses the only open page in the active session; if there are multiple, it errors and prompts.

## Why this is the canonical use case

Failed-action debugging is the universal pain point with agent-driven browser automation. The semantic tree shows you what the page *says* it is; the inspection surface shows you what the page *actually did* in the second between your action and your re-read. If you don't reach for this skill in real failure cases, the M5.7 inspection surface didn't land.
