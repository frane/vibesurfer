---
name: vibesurfer
version: 0.2.1
binary: vs
description: Agent-native headless browser. 26 primitives over a Unix-socket wire protocol. Real WKWebView (macOS), WebKitGTK 6 (Linux), or WebView2 (Windows) — all three engines verified per-commit by a real-browser integration suite. Optimistic concurrency via state tokens; tree-delta wire format; durable session/page/auth state in SQLite.
---

# vibesurfer (binary: `vs`)

`vs` is a stateless CLI that talks to a daemon (`vs serve`, auto-spawned on first call) over a Unix socket. Not installed? `npx vibesurfer <args>` runs the same binary (downloaded and cached on first use); `vs skill install` wires it into every detected agent. The daemon owns one long-lived browser engine on the OS main thread and a SQLite store at `~/.vibesurfer/state.db`. Every primitive writes one audit row before returning — there's no opt-out, no untracked operation.

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

## The 26 primitives

Wire form is `vs_<name>` (over the socket); CLI subcommand is `<name>` with hyphens. Each call returns a state envelope (`@<token>` success, `! CODE` error, `? warning` lines before the envelope).

### Lifecycle (1–4)

| # | CLI | What |
|---|-----|------|
| 1 | `vs session-open [--policy=NAME]` | Create a session. Writes `~/.vibesurfer/active-session`. |
| 2 | `vs session-close` | Close the active session. |
| 3 | `vs open <URL>` | Open a page in the session. |
| - | `vs goto <PAGE> <URL>` (`g`) | Navigate an existing page in place. Reuses the web view, so it skips browser spin-up and is much faster than open for successive navigations. Refs are fresh afterward. |
| - | `vs flow run <FILE>` | Run a declarative flow: a JSON array of steps, each an array of `vs` args. Runs in one session; `$page` expands to the last opened/navigated page, `$token` to its current token (fetched as needed). Stops at the first failing step. |
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

### Trusted typing (`vs type`, v0.1.27+; macOS)

`vs type <PAGE> <TEXT> [--secret] [-M mode]` (MCP `vs_type`) sends real per-character key events (KeyDown/KeyUp NSEvent) into the FOCUSED element, so the page sees `isTrusted=true` keydown -> beforeinput -> input. Rich-text editors (DraftJS/ProseMirror/contenteditable) and framework-controlled inputs need this — `act fill` uses the prototype-setter path, which those editors ignore. Place the caret first (`vs click-at` on the field). `--secret` redacts the text in the audit log (length only). Same `-M {human,careful,robotic}` cadence as the cursor primitives. macOS only for now (the keyboard path is not yet wired on the Linux XTest/libei and Windows SendKeyboard dispatchers — they return `! ENGINE_UNSUPPORTED`; use `act fill` there for plain inputs).


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
| 26 | `vs prompt-form <PAGE> --field <REF>=<LABEL>[,secret] ... --token=<TOK> [--open] [--no-wait]` | `pf` | Ask for several values at once via a browser form. Prints a single-use `http://127.0.0.1:…/entry/<nonce>` URL, parks until the human submits, then fills each ref in order. `--open` launches the default browser; `--no-wait` returns `form`+`url` immediately (park later via the MCP wait tool). |

When you need a secret the HUMAN holds — their real login, a TAN, a card number, anything you must never see — do not `vs act fill` it. Route it through the human: `vs prompt-input <PAGE> <REF> --message="<label-from-snapshot>" --secret --token=<TOK>` (one field) or `vs prompt-form` (a whole login), so the value goes human -> daemon -> page and never enters your context. Include enough context in the message that they know which field they're filling.

But a credential YOU own is not a secret to protect from yourself: a test user you just created, a seeded fixture account, a value from your own env/config. Fill those directly with `vs act fill` (plain inputs) or `vs type` (rich-text/framework inputs) — do NOT bounce them through `prompt-input`. Asking a human to type back a password you invented is pure friction, not security.

**No tty — MCP or non-interactive CLI (v0.1.12+ MCP, v0.1.20+ CLI):** without a controlling tty (`vs mcp`, or `vs prompt-input` from an agent's shell), the call enqueues a pending entry on the daemon and parks waiting for the value. The local user runs `vs pending list` (alias `pe ls`) to see what's queued and `vs pending fulfill [<id>]` (`pe f`) to type the value at their local tty — `vs pending fulfill` with no id auto-picks the single pending entry. `vs pending cancel <id>` (`pe c`) aborts. Once fulfilled, the agent's MCP tool call returns the new state token exactly as it would have for the local-CLI path.

`vs prompt-scan <PAGE> [--open]` (alias `ps`) shows the human a live view of the headless page (a QR code, a 2FA screen) and blocks until they press Enter, so out-of-band steps like scanning a TOTP enrollment QR work. Over MCP, compose the existing tools instead: call `vs_watch` for the live URL, relay it, then `vs_prompt_confirm` to wait.

**Browser entry (v0.1.23+):** the human alternative to the tty. `vs pending url` (`pe u`) mints a single-use loopback URL; the page lists every pending entry as one form (secret fields masked, password managers can autofill), and one submit fulfills them all. `vs prompt-form` prints such a URL automatically. Whole-login flow over MCP: call `vs_prompt_form` with all fields (`[{ref, label, secret}]`) — it returns `form` + `url` immediately; relay the URL to the user verbatim; then call `vs_prompt_form_wait` with the form id, which parks until submit and fills every ref in order. Values go browser → daemon → page; the agent never sees them. URLs are 127.0.0.1-only, 256-bit-nonce capability links, valid 10 minutes, consumed on submit.
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
| 16 | `vs capture <PAGE> [<REF>] [--full-page] [--base64]` | PNG to `~/.vibesurfer/captures/`. With `--base64` (`--b64`) the response body carries `base64=<bytes>` + `path=…` (default ON over MCP, where the pixels arrive as a proper image content block, not text). The dir is auto-capped (newest 200 / 30 days) after each shot; `vs capture clean [--all] [--older-than 7d] [--keep 50]` prunes it on demand. |
| 16b | `vs download <PAGE> [<URL>] [--dest=<P>] [--list] [--id=N]` | Save a file out of the page to `~/.vibesurfer/downloads/`. Alias `dl`. |

**Downloads (v0.2.1+).** A headless web view has no download UI, so a file the page tries to save has nowhere to go. `vs download` is the way out, in two modes:

- **You know the URL** — `vs download <PAGE> <URL>` reads it *from inside the page*, so session cookies, referer, and same-origin rules apply. This is how you get a PDF that only an authenticated session may fetch. Relative URLs resolve against the current document.
- **The page saved it itself** — `vs download <PAGE>` with no URL drains the newest download the page started: a `download` link, a viewer's Save button, a `blob:` navigation. Those are captured as they happen (bytes and all, even when the page revokes the object URL a tick later), so click Save first, then call `vs download`. `--list` shows what is waiting; `--id=N` picks one entry instead of the newest.

The response body is `path` / `size` / `mime` / `url` rows — bytes never cross the wire. A failed read (401, revoked blob, over the 64 MiB cap) comes back as an error saying why, not as silence. Files are named from `Content-Disposition` or the `download` attribute, sanitized to a single path component; a repeat download gets a `-1`, `-2`, … suffix rather than overwriting. `--dest` overrides the name (relative paths stay inside the downloads dir).

An `<iframe>` shows up in the tree as an `ifr` node whose label is its resolved `src` — the walker cannot cross into the frame, so that URL is what you feed `vs download`.

In MCP Apps hosts (Claude Desktop, ChatGPT, VS Code Copilot), calling `vs_watch` also renders a live panel inline: the tool carries `_meta.ui` → `ui://vibesurfer/live-panel`, a self-contained page that polls frames over the bridge via the app-only `vs_live_frame` tool (never billed to the model). Hosts without Apps support just get the URL line.

`vs watch <PAGE> [--open]` prints a read-only live-view URL (`http://127.0.0.1:…/live/<nonce>`, 30 min): an HTML page showing ~1 fps screenshots of the page while open. Relay it so the human can watch the browser work; MCP tool `vs_watch` returns the same `url` line. Frames are transient — no capture files, no audit rows.

`vs record start <PAGE> [--fps N] [--width PX] [--retina]` records the page to an H.264 MP4 while the agent works, capturing a frame at every mouse move, keystroke, and click so the video shows continuous motion (the cursor is composited on, since a headless snapshot carries no OS pointer). Frames are downscaled to 960px wide by default; `--width` picks another size, `--retina` keeps full device resolution. `vs record stop <PAGE>` (alias `rec`) flushes and prints the path (`~/.vibesurfer/captures/rec-<PAGE>.mp4`). `--fps` is 1..=30 (default 24). Encoded in real time with openh264 and muxed by pure-Rust muxide, so the MP4 plays natively everywhere with no ffmpeg. One recording per page. MCP: `vs_record_start` (`{page, fps?, width?}`) and `vs_record_stop` (`{page}`), both returning `path\t<file>`.


Over MCP, `vs_act` and `vs_open` take `capture: true` to attach a ~400px JPEG thumbnail image block to the result (~100 vision tokens) — visual confirmation without a separate capture round-trip. `VS_THUMBS=1` on the `vs mcp` process forces it on for every act/open (set it in the MCP server config for a visual transcript; costs tokens per action). CLI equivalent: chain `vs capture` when needed.
| 19 | `vs auth save\|load\|list\|clear <PAGE> <NAME>` | Per-origin cookie+storage blob, AES-256-GCM at rest. |
| - | `vs auth import <NAME> <FILE>` | Import a session captured elsewhere (passkey fallback): log in with a passkey in a real browser, export cookies + local/session storage as a v2 auth-blob JSON, import it, then `auth load` injects it into a headless page. |
| - | `vs auth webauthn <PAGE>` | Install a virtual WebAuthn authenticator on the page: a pure-JS ES256 software authenticator (no CDP) so passkey registration and login work headlessly. Enable it, then navigate/act as normal; the site's own create()/get() succeed. |


## Optimistic concurrency

Interactive refs the walker cannot see or hit (invisible / zero-size — sites keep hidden duplicates of buttons) carry `hid=1` in the tree; acting on one warns `? hidden_target ref=N`. Prefer the visible duplicate. Sessions and pages survive daemon restarts (rebuilt from SQLite at startup; engine pages recreated lazily on first use; re-`view` for a fresh baseline). Set `VS_CALLER=<stable-name>` in your env to keep the same session across YOUR restarts too — without it, session affinity is keyed to your process id and dies with it.

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

`vs auth save` runs an injected JS that snapshots `document.cookie`, `localStorage`, `sessionStorage` to JSON, then encrypts it with the master key (OS-keyring entry, or the fallback file `~/.vibesurfer/key` — auto-generated on first `vs serve` if neither exists; the file accepts 32 raw bytes, 64 hex chars, or base64 of 32 bytes). On `load`, the daemon re-runs the JS in the inverse direction.

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
| `~/.vibesurfer/downloads/` | Files saved by `vs download`. Never auto-pruned — they are deliberate artifacts, not scratch. |
| `~/.vibesurfer/skills/` | Composed skill bundles, listed by `vs skill list`. |
| `~/.vibesurfer/active-session` | Plain-text id of the active session. |
| `~/.vibesurfer/key` | Master key fallback, auto-generated by the daemon if no system keyring entry. 32 raw bytes; 64-hex or base64 text also accepted. |
