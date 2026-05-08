# Why vibesurfer

## The premise

CDP was designed for humans staring at DevTools. The browser-driving stacks built on top of it — Playwright, Puppeteer, browser-use, Stagehand — inherited that audience. They send big JSON payloads on every read, walk an HTML accessibility tree a thousand nodes deep, expose async events the caller has to choreograph, and have no opinion on auditability or replay.

That tooling fits a person attached to a debugger. It does not fit an LLM that pays per token, blocks per response, and has to plan a multi-step interaction without re-reading the entire page after every click.

vibesurfer is what the protocol looks like when the caller is the agent.

## What changes when the caller is an agent

**Tokens are the budget.** A view that returns 2,000 tokens of HTML noise on every call burns the agent's context window long before the task is done. vibesurfer's `vs view` returns a typed accessibility tree — role + label + ops + stable ref — at roughly a tenth the size of Playwright's accessibility snapshot for the same page. Subsequent views are deltas against the last-emitted tree, not full re-snapshots.

**Round trips are blocking.** Async event APIs assume the caller has an event loop and can interleave work while the page settles. An agent does not have an event loop; it has a synchronous turn. vibesurfer is line-oriented and synchronous: one request, one response, full envelope including the next state token.

**State drift is silent.** A page can mutate between the snapshot the agent reads and the click it tries to send. With CDP the click usually goes to the wrong element and produces a wrong screenshot; the agent has no way to know. vibesurfer requires every write to thread the page's current `state_token`. A stale write is rejected with the new tree attached, in a single round trip. Read-then-write is unnecessary; the engine reports drift instead.

**Refs survive re-renders.** A button that survives a re-render keeps the same `Ref(N)` across snapshots. A multi-step plan written against the first snapshot doesn't have to rediscover the world after every action.

**Idempotency is on by default.** `vs_act` keys on `(page, before_token, args_hash)` for 30 seconds. Repeating an action on a flaky network is free; it's not a doubled click.

**Composites collapse two-call sequences.** `vs open --view`, `vs view --layout=…`, `vs view --read=…`, `vs act --view`, `vs wait --view` exist because the canonical sequences ("open then view", "act then view") are observable on the wire and belong on the wire side, not the agent side.

**Audit comes free.** Every primitive writes one row to the `actions` SQLite table before returning. Replay, debugging, compliance, postmortems — all want the same table. There is no opt-out.

## What stays the same

The engine is a real browser. WebKit on macOS via `objc2`, WebKitGTK 6 on Linux via `webkit6`, WebView2 on Windows via `webview2-com`. Pages render the same way they would in a regular WebKit-based browser; JS runs the same way; auth flows behave the same way. Sites that depend on real CSS layout, real font measurement, real IndexedDB, real WebView cookie persistence — those work, because the engine is the same code path the user's browser uses.

What differs is the surface the *agent* sees. The page is the same; the protocol around it is built for the agent.

## The comparison table, with numbers

|  | Wire shape | Tokens per turn (steady state) |
|---|---|---|
| Playwright over CDP | async events, target juggling | ~2,000 |
| Lightpanda | CDP, less RAM | ~2,000 |
| Browser Use / Stagehand | a11y tree over CDP | ~800 |
| **vibesurfer** | **sync line protocol, deltas, state tokens** | **~50** |

"Steady state" means a fully-loaded page on which the agent has already seen a baseline snapshot. The first turn is a full tree on every system; the agent ergonomics question is what the tenth turn costs you.

## What this is not

vibesurfer is not a CDP shim. There is no `Page.navigate` underneath; the agent never sees devtools-protocol vocabulary. The inspector capture (`vs_inspect`) reads from a per-page ring buffer populated by a JS bridge installed at document-start, not from the WebKit Inspector protocol. ([ADR 0008](decisions/0008-inspector-protocol-internal-only.md).)

vibesurfer is not a Chromium driver. The engines are platform-native WebKits. Sites that work in Safari and Edge work; sites that depend on Chrome-only behaviour might surface real cross-browser bugs.

vibesurfer is not a WebMCP server. WebMCP assumes the site declares its tools to the agent; vibesurfer drives the site whether it wants to be driven or not. The agent's contract is with vibesurfer, not the site. (`vs_extract webmcp` exists for sites that *do* declare tools, but it's one schema among five, not the protocol.)

## What you give up

- Some Chrome-specific debugger features have no equivalent — coverage profiling, layer borders, the JavaScript heap snapshot view. The cases where these matter to an agent's task are rare; the inspector subcommands cover console, network, eval, storage, scripts, dom, performance.
- Real WebKit on Windows is WebView2 (Edge-Chromium underneath), so the macOS/Linux WebKit and the Windows WebView2 aren't byte-identical. They are the same content the user's browser would see on each platform; that's a deliberate choice, not a regression.
- The protocol is opinionated. If you want a JavaScript driver against a real Chrome that you can step through with breakpoints, this is not that tool. CDP is fine; use it.
