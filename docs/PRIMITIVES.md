# Primitives

This document is the per-primitive specification for the 19 **core**
primitives (the M0–M5 protocol surface) — request shape, response
shape, side effects, and audit row contents. Later releases added
more wire primitives that are not yet specified here: `vs_inspect`
(v0.1.8, eleven subcommands), the four cursor primitives `vs_move_to`
/ `vs_click_at` / `vs_hover_at` / `vs_drag` (v0.1.8+),
`vs_prompt_input` (v0.1.9), and the pending queue
`vs_prompt_input_queue` / `vs_pending_*` (v0.1.12) — 29 wire
primitives in total as of v0.1.13. Until they're specified here, the
bundled [SKILL.md](../crates/vs-cli/SKILL.md) and the
[CHANGELOG](../CHANGELOG.md) are their reference.

For the wire envelope, framing, tree format, delta ops, and state-token
mechanics, see [`PROTOCOL.md`](PROTOCOL.md). For role/error/warning code
tables, see [`codes.md`](codes.md).

> **Status.** Drafted at M0. Behavior lands across M2 (`vs-store`),
> M3 (engine), M4 (daemon, primitives 1–9), and M5 (daemon, primitives
> 10–19). Sections marked **TBD** are deferred to the relevant
> milestone.

## Conventions

- "Token in" means the request requires the most recent `state_token`
  for the implicated page.
- "Token out" means the success envelope carries a fresh `state_token`.
- Every primitive writes one row to `actions` before returning. The
  audit row always carries `primitive`, `args_redacted`, `args_hash`,
  `latency_ms`, `started_at`, `finished_at`, and the appropriate
  `before_token` / `after_token` / `error_code`.

---

## 1. `vs_session_open [--policy=<name>] [--isolated] [--intercept]`

Open a new session. Returns the session id and a fresh `state_token` for
the empty session.

`--isolated` gives the session its own in-memory cookie and storage jar,
shared by its pages and no one else, discarded on close. The flag is
kept on the session row, so a daemon restart rebuilds the pages isolated
but with empty storage. Without it, pages use the shared persistent jar.

`--intercept` routes the session's pages through a daemon-side HTTP
proxy, so the `vs_net_*` primitives (19b) can observe and route every
request. It implies a private in-memory jar, as `--isolated`: each
engine binds proxy settings to the data store. The flag is kept on the
session row; after a restart a fresh proxy starts with the first page.

- Token in: —
- Token out: yes
- Side effects: writes a row in `sessions`; updates
  `~/.vibesurfer/active-session`.

## 2. `vs_session_close`

Close the active session. Flushes pending audit rows, closes all open
pages, marks the session `closed`.

- Token in: —
- Token out: —
- Side effects: `sessions.status='closed'`, `pages.closed_at` populated.

## 3. `vs_open <url>`

Create a page in the active session and navigate to `<url>`. Returns the
page id and the initial `state_token`.

- Token in: —
- Token out: yes (page-scoped)
- Side effects: row in `pages`; first ref allocations land in `refs`.
- Warnings: `nav <url>` if the page redirected on load.

## 4. `vs_close [page]`

Close a page. Default is the current page.

## 5. `vs_view [page]`

Return the a11y tree for `<page>`. First call: full tree. Subsequent
calls: delta against the last-emitted tree for `(page, agent)`.

- Token in: —
- Token out: yes
- Body: tree or delta operations (see [`PROTOCOL.md`](PROTOCOL.md)).

## 6. `vs_read <ref>`

Return the full text/markdown of `<ref>`'s subtree. Used when the label
in the tree was truncated or when the agent needs the unquoted full
text.

- Token in: optional (if supplied, validated)
- Token out: yes

## 7. `vs_act <ref> <op> [val]`

Perform `<op>` on `<ref>`. Operations: `click`, `fill`, `scroll`,
`key`, `submit`, `hover`, `focus`. `fill` and `key` take `<val>`.

- Token in: required
- Token out: yes
- Idempotent on `(page_id, before_token, args_hash)` for 30s.
- Errors: `STALE_TOKEN`, `NOT_FOUND ref=<n>`, `POLICY_DENY`,
  `CONFIRM_REQUIRED`, `TIMEOUT`.
- New windows (v0.2.10+): a `target="_blank"` link or form, or
  `window.open()`, loads in this page, and `act` returns after that
  navigation. `click-at` and `type` do the same. The WebKit engines
  keep a POST body; WebView2 only exposes the URL, so on Windows the
  form arrives as a GET.

## 8. `vs_find <query>`

Search across all pages in the session. Returns a list of `(page_id,
ref)` matches with surrounding context.

## 9. `vs_wait <cond> [val] [--timeout=<ms>]`

Block until `<cond>` is met or the timeout fires. Conditions: `stable`,
`net-idle`, `ref <ref>`, `gone <ref>`, `text <text>`, `token-change`.
Default timeout 5000ms; daemon caps at 60000ms.

## 10. `vs_extract <schema>`

Extract structured data using a known schema (`table`, `form`, `list`,
`jsonld`, `webmcp`) or a path to a user-defined schema. Returns one
record per match.

- Token in: required
- Token out: yes

## 11. `vs_mark <ref> <name>`

Persist `<ref>` as a named anchor for the session, stored by DOM path
in `marks`. Marks survive page mutations as long as the path resolves;
otherwise reads return `! NOT_FOUND mark=<name>`.

- Token in: required
- Token out: —

## 12. `vs_annotate <target> <key> <val>`

Attach a `(key, value)` to `<target>`. Target shapes:
`ref:<n>`, `mark:<name>`, `page`. Annotations are stored in
`annotations`; they are agent-visible state, not engine state.

## 13. `vs_status`

Single-block summary of the session: open pages, last action, marks,
recent warnings.

## 14. `vs_log [--since=...] [--group=...] [--page=...]`

Slice the audit log. One action per line. Default: last 50 actions in
the active session.

## 15. `vs_skill <name> [args...]`

Run a composed skill from `skills/<name>/`. v1 skills are linear
scripts; branching is deferred. `vs_skill list` and `vs_skill show
<name>` are the discovery primitives.

## 16. `vs_capture [ref] [--viewport=...] [--full-page] [--format=...]`

Screenshot. Default scope is the visible viewport; pass a `<ref>` to
capture a specific element, or `--full-page` for the entire page. The
binary lands at a daemon-chosen path; the response carries the path,
not the bytes.

## 16b. `vs_download <page> [url] [--dest=P] [--list] [--id=N] [--timeout-ms=N]`

Save a file out of a page. Like `vs_capture`, the response carries the
on-disk path, not the bytes.

A headless web view has no download delegate, so a download the page
starts has nowhere to land and no event an agent can observe. Both
halves of this primitive exist to close that hole:

- **`url` given** — read it from *inside* the page, so the request
  carries the page's cookies and referer. Relative URLs resolve
  against the current document. This is the path for a file behind a
  login, and for the contents of an `<iframe>` (whose `src` appears as
  an `ifr` node's label in `vs_view`).
- **`url` omitted** — drain the newest download the page performed
  itself. Every frame runs a document-start shim that intercepts
  `download` anchors, `blob:` / `data:` navigations, and
  `window.open` of either, reads the bytes at intercept time, and
  parks them in a bounded per-page buffer. Reading at intercept time
  is what makes the common viewer pattern work: a saved blob's object
  URL is typically revoked a tick after the click, so anything that
  waits and fetches later finds nothing. `--id=N` selects a buffered
  entry; `--list` enumerates them without reading any payload.

Response body rows: `path`, `size`, `mime`, `url`. `--list` emits one
`dl\t<id>\t<size>\t<ready|pending>\t<mime>\t<name>\t<url-or-error>` row
per entry.

Filenames come from `Content-Disposition`, the anchor's `download`
attribute, or the URL's last path segment — all page-controlled, so
each is reduced to a single path component before the daemon writes
anything. A name already taken gets a `-1`, `-2`, … suffix instead of
overwriting. `--dest` overrides the name; a relative `--dest` resolves
under the downloads directory and cannot climb out of it.

Downloads over 64 MiB are refused, and a failed read (HTTP error,
revoked blob, size cap) returns an error naming the cause rather than
succeeding with nothing.

## 17. `vs_viewport <preset|WxH> [--dpr=N]`

Set the page's viewport. Persistent for the page until changed again.
Triggers a re-baseline (next `vs_view` is a fresh full tree). Presets
are listed in [`codes.md`](codes.md). DPR defaults to 2.

## 18. `vs_layout <ref>...`

Computed box, visibility, and z-index for one or more refs. The visual
counterpart to `vs_view`'s structural data.

## 19. `vs_auth <save|load|list|clear> [name]`

Persistent, encrypted auth state.

- `save <name>`: dump cookies (via the host-side cookie store, so
  HttpOnly cookies are included), localStorage, and sessionStorage —
  IndexedDB is **not** captured; encrypt with AES-256-GCM via `ring`
  (key from the OS keyring, falling back to `~/.vibesurfer/key`;
  if neither exists the daemon generates a key on startup and writes
  it to the fallback file, mode 0600. A hand-written key file may be
  32 raw bytes, 64 hex chars, or base64 of 32 bytes);
  insert into `auth_blobs`.
- `load <name>`: reverse. Emits `? auth_loaded <name>` and forces a
  fresh `vs_view` baseline on next call.
- `list`: enumerate stored blob names.
- `clear <name>` or `clear --all`: remove blob(s).

Auth blobs are scoped per machine (the encryption key is local).
Cross-host portability is not a v1 feature.

## 19b. `vs_net_*`: request interception

Only for `--intercept` sessions; any other session gets `BAD_REQUEST`.
All take the session from `--session`. None of them write audit rows.

Each intercepting session has its own proxy on `127.0.0.1:<ephemeral>`;
the port tells the daemon which session a connection belongs to.

| Engine | How pages reach the proxy | Loopback |
|--------|---------------------------|----------|
| macOS WebKit | `WKWebsiteDataStore.proxyConfigurations` with an HTTP CONNECT proxy (macOS 14+), failover off | `127.0.0.1`, `localhost`, `::1` and the host's own addresses always go direct. `*.localhost` names are proxied; the proxy resolves them to loopback. |
| WebKitGTK 6 | `NetworkSession::set_proxy_settings`, custom `http://127.0.0.1:<port>`, empty ignore list | Proxied. |
| WebView2 | `--proxy-server=http://127.0.0.1:<port> --proxy-bypass-list=<-loopback>` on a per-session environment and user data folder | Proxied. |

http and https are both intercepted. https arrives as `CONNECT
host:port`; the proxy answers the browser's TLS with a leaf certificate
for the host (SNI when sent) signed by the daemon's CA, offers only
http/1.1 in ALPN, and handles the requests inside like plain ones, with
`https://` URLs (`:443` left out). On macOS plain http also arrives as
`CONNECT`; the proxy reads the HTTP inside the tunnel. WebSocket
upgrades (`ws:` and `wss:`) are forwarded and then piped.

The CA is made on first use and kept as `~/.vibesurfer/intercept-ca.pem`
and `intercept-ca.key` (0600). Every leaf uses one key pair made at
daemon start. Upstream TLS is verified against the Mozilla roots
(webpki-roots) plus any PEM certificates in the file named by
`VS_INTERCEPT_EXTRA_ROOTS` in the daemon's environment; a failed
upstream handshake is a `fail <id> tls: ...` and the browser's request
fails. A fulfilled request never touches upstream, so it works for a
host that does not exist.

Each engine trusts the CA for intercept sessions only:

| Engine | Trust |
|--------|-------|
| macOS WebKit | The page's navigation delegate answers `NSURLAuthenticationMethodServerTrust` challenges: the CA is added as an anchor (system anchors kept) and the trust evaluated again. Every TLS connection of the page gets the challenge, subresources (fetch, XHR, images) included. |
| WebKitGTK 6 | The session's network session uses `TLSErrorsPolicy::Ignore`. Leaves are made per host on demand, so they cannot be allowed one by one up front; every TLS connection of the session ends at the proxy, which verifies upstream. |
| WebView2 | `--ignore-certificate-errors-spki-list=<leaf key>,<CA key>` (base64 SHA-256 of each SPKI) on the session's environment, and a `ServerCertificateErrorDetected` handler that allows the error should Chromium still raise one. |

### `vs_net_poll [--wait-ms=N]`

Returns and clears the queued events, oldest first. With `--wait-ms`
(capped at 30000) it blocks until the first event or the timeout.

```
req <id> <METHOD> <url> type=<type> nav=<0|1> [frame=<main|sub>]
h <name>: <value>
body <base64>
res <id> <status> <url> <reason>
h <name>: <value>
fin <id> size=<n>
fail <id> <error>
drop <n>
```

`h` lines follow their `req` or `res` line, in the order received;
`body` appears only for a request with a body. `res` carries the URL
the browser asked for, even when `continue --url` sent it elsewhere.
`fin` counts body bytes as received (still content-encoded). `drop`
says how many events fell off a full queue (20000).

Ids are per session, starting at 1. A redirect is a `res 3xx` and a
new `req` for the next hop. Fulfilled requests get `res` and `fin`;
aborted ones and network errors get `fail`.

`type` is inferred: `Sec-Fetch-Dest` when present (Chromium always;
WebKit only to trustworthy origins such as `*.localhost`), else
`Accept` (`text/html` document, `text/css` stylesheet, `image/*`
image), else the URL extension, else `other`. Types: `document`,
`script`, `stylesheet`, `image`, `font`, `media`, `fetch`, `websocket`,
`manifest`, `other`. `fetch` covers fetch() and XHR alike. `frame` is
set for documents, `sub` only when `Sec-Fetch-Dest` says iframe.

### `vs_net_route on|off`

`on` holds every request after its `req` event until the client
decides. A request held 30 s without a decision continues unchanged.
`off` continues everything held.

### `vs_net_continue <id> [--url=U] [--method=M] [--headers=H] [--body-b64=B]`
### `vs_net_fulfill <id> [--status=N] [--headers=H] [--body-b64=B]`
### `vs_net_abort <id> [--error=NAME]`

Decide a held request. `--headers` is `name: value` lines joined by
newlines (the CLI's repeatable `--header` builds it); when given it
replaces the whole header list. Fulfill defaults to status 200 and an
empty body; Content-Length is set by the proxy. Abort fails the
browser's request and reports `fail <id> <NAME>` (default `failed`).
The browser must not retry it: CFNetwork (macOS) retries an idempotent
request whose connection closes with no response, so on macOS the proxy
first sends a status line with a malformed header field, a protocol
error it does not retry; elsewhere it closes the connection. `NOT_FOUND request=<id>` if the request is not
held (unknown, decided, or timed out).

### `vs_net_fetch <id> [--url=] [--method=] [--headers=] [--body-b64=]`

Performs the held request (with the overrides) and returns the
response without delivering it; the request stays held, so the client
then fulfills, continues or aborts it.

```
res <id> <status> <url> <reason>
h <name>: <value>
body <base64>
```

### `vs_net_body <id>`

`body <base64>`: a response body the proxy delivered (forwarded or
fulfilled). The newest bodies are kept, 16 MiB per session in total;
`NOT_FOUND body=<id>` once evicted.

---

## M5.5 amendments

### Short-form CLI aliases

Every primitive has a single- or two-letter visible alias for token economy in agent contexts (`ae`-style). Long forms remain for human readers.

| Long | Short | Long | Short | Long | Short |
|------|-------|------|-------|------|-------|
| `session-open` | `so` | `wait` | `w` | `skill` | `sk` |
| `session-close` | `sc` | `extract` | `x` | `capture` | `cap` |
| `open` | `o` | `mark` | `m` | `viewport` | `vp` |
| `close` | `c` | `annotate` | `an` | `layout` | `lay` |
| `view` | `v` | `status` | `st` | `auth` | `au` |
| `read` | `r` | `log` | `l` | | |
| `act` | `a` | `find` | `f` | | |

### Composite flags

See [ADR 0006](./decisions/0006-composite-flags-are-observation-driven.md) for the design rule. The five flags below collapse canonical two-call sequences into a single response.

- **`vs_open URL --view`** — bundles the post-open tree into the response. Page id from the open call is used automatically. Idempotent within 30s for repeat URLs (cached open + fresh view).
- **`vs_wait COND --view`** — wait then view. Skipped on `! TIMEOUT`. View token is post-wait (guaranteed fresh).
- **`vs_act REF OP --view`** — act then view. Skipped on `! STALE_TOKEN`, `! NOT_FOUND`, `! POLICY_DENY`. Idempotency cache hit on the act still runs a fresh view. Audit row records `--view` in `args_redacted`.
- **`vs_view PAGE --layout=N,M`** — bundles `getBoundingClientRect` boxes for the listed refs. Refs not in the current tree produce `! NOT_FOUND ref=N` lines but do not abort the view.
- **`vs_view PAGE --read=N`** — bundles full text of one ref's subtree. Unknown ref produces `! NOT_FOUND ref=N`; the view itself succeeds.

Internal implementation: each composite flag expands to a `Vec<Primitive>` against `Daemon::dispatch`. The wire still delivers one primitive per frame; v2 explicit pipeline syntax (ADR 0007) lands as a parser-only change in a later milestone.
