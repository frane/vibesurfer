# 0008 — Inspector protocol is an engine implementation detail, never a wire surface

**Status**: Accepted (M5.7).
**Context**: M5.7 introduces `vs_inspect` for console capture, network logs, eval, storage, scripts, DOM, and performance. On WebKitGTK (and to a lesser extent on Apple WebKit) the natural implementation route is the WebKit Inspector protocol — same family as the Chrome DevTools Protocol (CDP). Apple's DevTools speak it; WebKitGTK's `webkit_web_view_get_inspector` exposes it.

## Decision

The Inspector protocol is used **inside the engine implementation only**. It never appears on the agent-facing wire. Agents see `vs_inspect <kind>` and a small kind table; they never construct CDP messages, see CDP error codes, or care about Inspector domains.

Concretely:

- `console`, `script <id>`, `scripts`, and (where viable) `network` use Inspector internally on Linux/WebKitGTK.
- On macOS, Apple's WebKit does not expose the Inspector protocol as a public API. `console` uses `WKUserScript` + `WKScriptMessageHandler`. `network` uses a `fetch` / `XMLHttpRequest` JS override (with documented limitations: `<img>`, `<link>`, navigation, beacons, EventSource, WebSocket are not captured). See `docs/known-issues.md`.
- The agent's view does not depend on which mechanism the engine used. Output formats are platform-neutral.

## Consequence

- We pay engine-specific implementation cost in exchange for a stable, small, agent-facing API. The Inspector protocol is a moving target; insulating agents from it means we can swap implementation strategies without breaking skills.
- `PROTOCOL.md` and `PRIMITIVES.md` never reference CDP/Inspector vocabulary. Examples use `vs_inspect`'s semantic surface only.
- The shape mirrors how vibesurfer's other engine FFI is structured: WebKit's accessibility APIs are an implementation detail of `vs_view`; Inspector is an implementation detail of `vs_inspect`.

## Out of scope

- Exposing CDP directly to agents. Even as an `--unsafe-cdp` escape hatch — the surface is too large and too unstable. Agents that need surgical access have `vs_inspect eval` (PR3).
- A protocol-conversion shim (`vs_inspect cdp <method> <params>`). No.
