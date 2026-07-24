# vibesurfer

**A browser for LLMs, not humans.** Headless WebKit with a line-oriented protocol built for agents in loops — no Chrome, no CDP.

Try it in one line, no install:

```
npx vibesurfer session-open
npx vibesurfer open https://example.com
```

The first run downloads the prebuilt binary for your platform, verifies its checksum, and caches it under `~/.vibesurfer/bin/`. Every later run is instant and offline.

## What it is

vibesurfer drives a real browser engine — WKWebView (macOS), WebKitGTK 6 (Linux), WebView2 (Windows) — over a tiny Unix-socket wire protocol designed for an agent stuck in a while loop, not a human staring at DevTools:

- **Accessibility-tree snapshots**, not CSS selectors. Each element is a stable integer `ref` you act on ten turns later without re-reading the page. Tree-*deltas* on repeat views keep token cost low.
- **Trusted native input.** Clicks and typing dispatch through real OS input events (`isTrusted = true`), so anti-bot systems can't tell them from a human.
- **Optimistic concurrency** via state tokens, **durable sessions** in SQLite (they survive daemon restarts), and per-origin encrypted **auth** state.
- **MCP server built in** (`vs mcp`) — wire it into Claude, Claude Code, Cursor, Codex, Gemini, Google Antigravity, and more with `vs skill install`.

## Common commands

```
npx vibesurfer --help            # every primitive
npx vibesurfer session-open      # start a session (auto-spawns the daemon)
npx vibesurfer open <url>        # open a page
npx vibesurfer view <page>       # snapshot the accessibility tree
npx vibesurfer act <page> <ref> click --token=<t>
npx vibesurfer skill install     # install into every detected AI agent
```

Prefer a permanent install? `brew install vibesurfer` (macOS/Linux), `cargo install vibesurfer`, or the [other options in the repo](https://github.com/frane/vibesurfer#install). The `npx` package pins its matching release, so `npx vibesurfer@<version>` runs exactly that build.

## Links

- **Repository & docs:** https://github.com/frane/vibesurfer
- **crates.io:** https://crates.io/crates/vibesurfer
- **License:** Apache-2.0
