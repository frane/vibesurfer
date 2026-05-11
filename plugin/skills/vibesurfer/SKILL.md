---
name: vibesurfer
version: 0.1.0
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

## The 19 primitives

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
| 16 | `vs capture <PAGE> [<REF>] [--full-page]` | PNG to `~/.vibesurfer/captures/`. |
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
| `wpe` (Linux) | ✅ | JS `el.click()` (untrusted) | ✅ | ✅ | ✅ | WebKitGTK 6 via `webkit6` crate. Needs `libwebkitgtk-6.0`. |
| `webview2` (Windows) | ✅ | JS `el.click()` (untrusted) | ✅ | ✅ | ✅ | Microsoft Edge / Chromium via `webview2-com`. |

Trusted clicks: only the macOS engine routes `vs act click` through native input dispatch so the resulting `MouseEvent` carries `isTrusted = true`. Linux + Windows still go through injected JS, which anti-bot fingerprinters (Cloudflare, Google, hCaptcha) will treat as automated. M7 wires both to their native equivalents (WebKitGTK GDK events and WebView2 CDP `Input.dispatchMouseEvent`).

`vs status` reports the active backend's capabilities; the CLI surfaces the protocol error `ENGINE_UNSUPPORTED` if you try a primitive the active backend doesn't implement.

## Where things live

| Path | What |
|------|------|
| `~/.vibesurfer/daemon.sock` | Unix socket the CLI talks to. |
| `~/.vibesurfer/state.db` | SQLite (sessions, pages, refs, marks, annotations, auth blobs, audit log). |
| `~/.vibesurfer/captures/` | PNG screenshots from `vs capture`. |
| `~/.vibesurfer/skills/` | Composed skill bundles, listed by `vs skill list`. |
| `~/.vibesurfer/active-session` | Plain-text id of the active session. |
| `~/.vibesurfer/key` | Master key (fallback if no system keyring). |
