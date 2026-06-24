---
name: vibesurfer
version: 0.1.15
binary: vs
description: Agent-native headless browser. 20 primitives over a Unix-socket wire protocol. Real WKWebView (macOS), WebKitGTK 6 (Linux), or WebView2 (Windows) — all three engines CI-verified by 48 integration cells per platform. Optimistic concurrency via state tokens; tree-delta wire format; durable session/page/auth state in SQLite.
---

# vibesurfer (binary: `vs`)

`vs` is a stateless CLI that talks to a daemon (`vs serve`, auto-spawned on first call) over a Unix socket. The daemon owns one long-lived browser engine on the OS main thread and a SQLite store at `~/.vibesurfer/state.db`. Every primitive writes one audit row before returning — there's no opt-out, no untracked operation.

## Short forms are the default in agent contexts

Every primitive has a short alias. Long forms exist for human-readable documentation; agent invocations should use the shorter form to save tokens. `vs o https://...` is the canonical shape for `vs open https://...`, not the other way around. The 19-primitive tables below lead with the short form and show the long form in parens; both work identically.

Frequent flags also have short forms: `-S` (`--session`), `-j` (`--json`), `-F` (`--full`), `-s` (`--since`), `-n` (`--limit`), `-P` (`--page`). The `--token` flag stays verbose because it's never typed by hand — you paste it from the previous read.

## Use this tool when

- You need to drive a real browser to read or interact with a web app (login, scrape behind auth, click through SPA state).
- You need stable refs across snapshots so you can plan a multi-step interaction without scraping coordinates.
- You want persistent auth (cookies + storage) that survives across sessions, encrypted at rest.
- You want every action audited automatically — for replay, debugging, or compliance.

## Don't use this tool for

- One-shot HTTP fetches with no DOM (use `curl`).
- Anything where you'd be parsing the rendered HTML by string matching — `vs_view` already gives you a typed accessibility tree with stable refs.
- Headless screenshots of fixed URLs with no interaction (overkill — though you can; see `vs capture`).

## The 25 primitives

Wire form is `vs_<name>` (over the socket); CLI subcommand is `<name>` with hyphens. Each call returns a state envelope (`@<token>` success, `! CODE` error, `? warning` lines before the envelope).

### Lifecycle (1–4)

| # | CLI | What |
|---|-----|------|
| 1 | `vs session-open [--policy=NAME]` | Create a session. Writes `~/.vibesurfer/active-session`. |
| 2 | `vs session-close` | Close the active session. |
| 3 | `vs open <URL>` | Open a page in the session. |
| 4 | `vs close <PAGE>` | Close a page. |

### Read (5–6, 13–14)

| # | CLI | What |
|---|-----|------|
| 5 | `vs view <PAGE> [--full]` | A11y tree. First call after `open` is full; subsequent calls are deltas. |
| 6 | `vs read <PAGE> <REF>` | Full text of one ref. |
| 13 | `vs status` | Active session + open pages summary. |
| 14 | `vs log [--page=<P>] [--group=<G>] [--since=<EPOCH>] [--limit=N]` | Audit log slice. |

### Mutate (7, 9–12, 17)

| # | CLI | What |
|---|-----|------|
| 7 | `vs act <PAGE> <REF> <OP> [VALUE] --token=<TOK> [--group=<LABEL>]` | Click / fill / scroll / key / submit / hover / focus. Token from previous read. |
| 9 | `vs wait <PAGE> <COND> [VALUE] --timeout=<MS>` | `stable` / `text` / `ref-appears` / `ref-gone`. |
| 11 | `vs mark <PAGE> <REF> <NAME> --token=<TOK>` | Persist a ref as a named anchor. |
| 12 | `vs annotate <TARGET> <KEY> [VALUE]` | `ref:N` / `mark:NAME` / `page` annotation. |
| 17 | `vs viewport <PAGE> <SPEC> [--dpr=N]` | Preset (`mobile` / `desktop` / etc.) or `WxH`. Re-baselines next view. |

### Cursor coordinates (20–23, v0.1.8+; trusted on all platforms in v0.1.11+)

Coordinate-addressed input with native trusted dispatch on every backend. macOS uses `NSEvent`, Linux uses XTest via the pure-Rust `x11rb` client (or libei via xdg-desktop-portal RemoteDesktop on pure Wayland), Windows uses `SendMouseInput` on `ICoreWebView2CompositionController`. Every resulting `MouseEvent` carries `isTrusted = true` in JS — Cloudflare / Google / hCaptcha can't tell the click from a real cursor. All four primitives take `--mode={human,careful,robotic}` (short `-M`), default `human`.

`human` synthesizes a Bezier path from the last known cursor position with Fitts-law arrival timing; the visible motion is indistinguishable from a real cursor reaching the target before the click. `careful` is a single-shot move. `robotic` is a teleport (no path).

| # | CLI | Short | What |
|---|-----|-------|------|
| 20 | `vs move-to <PAGE> <X> <Y> [-M=human]` | `mt` | Move the cursor to (x, y). No click. |
| 21 | `vs click-at <PAGE> <X> <Y> --token=<TOK> [-M=human]` | `ca` | Trusted click at (x, y) after a humanized lead-in. |
| 22 | `vs hover-at <PAGE> <X> <Y> [-M=human]` | `ha` | Hover at (x, y). |
| 23 | `vs drag <PAGE> <X1> <Y1> <X2> <Y2> --token=<TOK> [-M=human]` | `dr` | Press at start, drag along a humanized path, release at end. v0.1.11+ also synthesizes the HTML5 `DragEvent` chain (`dragstart` → `dragenter` → `dragover` → `drop` → `dragend` with a real `DataTransfer`) so react-dnd, native `draggable="true"` widgets, and React-Flow HTML5-backend nodes observe the drop. |


### Human-in-loop (24–25, v0.1.9+; MCP-aware in v0.1.12+)

For credentials, TANs, and any other value the agent must not see. The CLI reads from the local terminal the user is sitting at; the agent never receives the bytes.

| # | CLI | Short | What |
|---|-----|-------|------|
| 24 | `vs prompt-input <PAGE> <REF> --message="..." [--secret] --token=<TOK>` | `pi` | Print the message to the user, read a line (echo off when `--secret`), then fill it into the ref via the daemon's trusted-fill path. The agent that issued this call sees only `ok` + new token. |
| 25 | `vs prompt-confirm <PAGE> --message="..."` | `pc` | Block until the user presses Enter, or abort on Ctrl-C. Use as a gate before a mutating click ("about to transfer X — Enter to confirm"). |

When you need credentials, never call `vs act fill` with the value. Always call `vs prompt-input <PAGE> <REF> --message="<label-from-snapshot>" --secret --token=<TOK>` and let the user type. Include enough context in the message that they know which field they're filling (the field label from the snapshot is usually enough).

**MCP / Claude Desktop / Codex (v0.1.12+):** `vs mcp` has no tty, so the MCP version of `vs_prompt_input` enqueues a pending entry on the daemon and parks waiting for the value. The local user runs `vs pending list` (alias `pe ls`) to see what's queued and `vs pending fulfill [<id>]` (`pe f`) to type the value at their local tty — `vs pending fulfill` with no id auto-picks the single pending entry. `vs pending cancel <id>` (`pe c`) aborts. Once fulfilled, the agent's MCP tool call returns the new state token exactly as it would have for the local-CLI path.
### Search / extract (8, 10, 18)

| # | CLI | What |
|---|-----|------|
| 8 | `vs find <QUERY>` | Substring search across all open pages in the session. |
| 10 | `vs extract <PAGE> <SCHEMA> --token=<TOK>` | `list` / `table` (rest are `BAD_REQUEST` until written). |
| 18 | `vs layout <PAGE> <REF>...` | `getBoundingClientRect` per ref. |

### Capture / persist (15–16, 19)

| # | CLI | What |
|---|-----|------|
| 15 | `vs skill list \| show <NAME>` | List or show installed skill bundles. |
| 16 | `vs capture <PAGE> [<REF>] [--full-page] [--base64]` | PNG to `~/.vibesurfer/captures/`. With `--base64` (`--b64`) the response body carries `base64=<bytes>` + `path=…` for MCP-driven agents that want the pixels inline (default ON over MCP). The dir is auto-capped (newest 200 / 30 days) after each shot; `vs capture clean [--all] [--older-than 7d] [--keep 50]` prunes it on demand. |
| 19 | `vs auth save\|load\|list\|clear <PAGE> <NAME>` | Per-origin cookie+storage blob, AES-256-GCM at rest. |

## Optimistic concurrency

Every read returns a state token. Mutations require the token in `--token=<TOK>`. Stale token → `! STALE_TOKEN <new> <reason>`; you re-read and retry. There is no manual locking primitive. Don't bash-batch mutations against the same page without re-reading between them.

## Idempotency

If you re-issue the *exact* same `vs act` (same ref, same op, same value, same before-token, same group) within ~5 seconds, the daemon recognizes the replay and returns `? idempotent_hit` followed by the original success envelope — no double-click, no double-fill.

## Auth flow

```sh
# First time (browser, real human)
vs session-open
PAGE=$(vs open https://app.example.com)
# ...log in via the page...
vs auth save "$PAGE" example-prod    # persists cookies + localStorage

# Tomorrow
vs session-open
PAGE=$(vs open https://app.example.com)
vs auth load "$PAGE" example-prod    # restores the session
# you're logged in
```

`vs auth save` runs an injected JS that snapshots `document.cookie`, `localStorage`, `sessionStorage` to JSON, then encrypts it with the master key (keyring entry, or a fallback file). On `load`, the daemon re-runs the JS in the inverse direction.

## How the wire stays cheap

- **Tree deltas, not re-dumps.** First `vs view` after `open` returns the full tree. Subsequent calls return only what changed since the last token the agent saw.
- **Stable refs.** Every interesting element gets a sticky `data-vs-ref` integer that survives across snapshots — you can plan multi-step flows without re-discovering elements.
- **Tab-separated lines, not JSON.** Hot-path reads cost a fraction of equivalent JSON. Use `--json` only when you're inspecting by hand.

## Common mistakes to avoid

- **Don't omit `--token` on mutations.** It's not optional — the daemon will reject with `BAD_REQUEST` if missing.
- **Don't forget the session.** `vs --session=<id>` overrides; otherwise it reads `~/.vibesurfer/active-session`.
- **Don't run multiple `vs serve` instances.** Auto-spawn picks up the existing socket; if you kill it manually, restart by running `vs serve` directly.
- **Don't expect engine-side timeouts to be exact.** `--timeout=5000` is a budget, not a deadline; the daemon may overshoot by a runloop tick (~50ms on macOS, ~10ms on Linux).

## Capabilities by platform

All three engines are verified in CI by the same 48-cell integration suite; the matrix below tracks the few axes where engine behavior differs in observable ways.

| Backend | Renders | Trusted clicks | Viewport | Layout | Auth | Notes |
|---------|---------|----------------|----------|--------|------|-------|
| `webkit` (macOS) | ✅ | ✅ via `NSEvent` | ✅ | ✅ | ✅ | System WebKit.framework, `WKWebView`. |
| `wpe` (Linux) | ✅ | ✅ via XTest (`x11rb`); libei (ashpd RemoteDesktop portal) on pure Wayland | ✅ | ✅ | ✅ | WebKitGTK 6 via `webkit6` crate. Needs `libwebkitgtk-6.0`. Pure Wayland without Xwayland and no portal → falls back to JS `el.click()` (untrusted). |
| `webview2` (Windows) | ✅ | ✅ via `SendMouseInput` on `ICoreWebView2CompositionController` | ✅ | ✅ | ✅ | Microsoft Edge / Chromium via `webview2-com`. DirectComposition target per page. |

Trusted clicks (v0.1.11+): every backend routes `vs act click` and the cursor primitives through native OS input dispatch so the resulting `MouseEvent` carries `isTrusted = true` — anti-bot fingerprinters (Cloudflare, Google, hCaptcha) cannot distinguish from a real cursor. The Linux libei path requires the user's compositor to support the RemoteDesktop portal and the user to grant a one-time consent prompt at process startup; detection falls through to XTest (X11 / Xwayland) and finally to untrusted JS `el.click()` if neither is reachable.

`vs status` reports the active backend's capabilities; the CLI surfaces the protocol error `ENGINE_UNSUPPORTED` if you try a primitive the active backend doesn't implement.

## Where things live

| Path | What |
|------|------|
| `~/.vibesurfer/daemon.sock` | Unix socket the CLI talks to. |
| `~/.vibesurfer/state.db` | SQLite (sessions, pages, refs, marks, annotations, auth blobs, audit log). |
| `~/.vibesurfer/captures/` | PNG screenshots from `vs capture`. Auto-capped (newest 200 / 30 days); prune with `vs capture clean`. |
| `~/.vibesurfer/skills/` | Composed skill bundles, listed by `vs skill list`. |
| `~/.vibesurfer/active-session` | Plain-text id of the active session. |
| `~/.vibesurfer/key` | Master key (fallback if no system keyring). |
