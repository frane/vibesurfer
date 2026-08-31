# vibesurfer wire protocol

This document is the **specification**. If implementation disagrees with what
follows, the implementation is wrong.

## Status

**Frozen at `proto=1`.** Drafted at M0, encoder/decoder landed in M1
(`vs-protocol` crate), semantics wired into the daemon in M4, and the
framing + token/delta semantics + [invariants](#invariants-proto1) are
now a stable contract (see Versioning). Additions (new flags, new
primitives) are allowed within `proto=1`; the frozen surface is not.

## Scope

The wire protocol governs how `vs` (the CLI) and `vs serve` (the daemon)
exchange requests and responses over a Unix domain socket. It is also the
interface any future MCP shim translates to.

## Design constraints

1. **Token-optimized.** The wire format is the cost model. JSON is forbidden
   on the hot path. Lines, tab-separated fields, and short codes only.
2. **Stateless CLI.** The CLI carries no state across invocations except an
   active-session pointer in `~/.vibesurfer/active-session`. All durable
   state lives in SQLite, owned by the daemon.
3. **Optimistic concurrency.** Reads return a `state_token`; writes require
   one. Stale tokens are rejected with `! STALE_TOKEN <new> <reason>`. The
   agent reads again and retries.
4. **No transactions.** Browser actions hit remote systems and cannot be
   rolled back. Atomicity is N/A; freshness via `state_token` is sufficient.
5. **Tree deltas, not re-dumps.** After the first `vs_view` on a page,
   subsequent reads emit only what changed since the last token the agent
   was shown.
6. **Stable refs.** Elements are addressed by integer refs that are stable
   for the lifetime of the session. Refs are never reused.
7. **Audit-by-construction.** Every primitive call writes one row to
   `actions` in SQLite before returning. There is no opt-out.

## Framing

- Transport: AF_UNIX socket at `~/.vibesurfer/daemon.sock` on Unix; a Windows named pipe on Windows. The CLI resolves either from the same `Path`.
- Encoding: UTF-8.
- Line terminator: `\n` (LF). No `\r\n`.
- Request: a single line.
- Response: one or more lines, terminated by a blank line (`\n\n`).
- Binary payloads (screenshots, downloads): written to disk by the daemon;
  the response references the path. Never inlined.

## Request line

```
<primitive> [arg]... [--flag[=val]]...
```

- Primitive names are lowercase with `vs_` prefix (e.g. `vs_open`, `vs_act`).
- Positional args precede flags.
- Args containing whitespace are bare-quoted with `"..."` — no escape
  characters. Inner quotes are not allowed; agents read the full value via
  `vs_read <ref>` instead.
- Flags use long form (`--name` or `--name=value`). No short flags on the
  wire (the CLI may accept short flags and translate).

Examples:

```
vs_session_open --policy=default
vs_open https://example.com
vs_view
vs_act 7 click
vs_act 2 fill "frane@example.com"
vs_capture --viewport=mobile --full-page
```

## Response envelope

The first non-warning line of every response is the **envelope**:

| Sigil | Meaning |
| ----- | ------- |
| `@<token>` | Success. `<token>` is the new `state_token` (16 hex chars, blake3 truncated). |
| `! <CODE> [arg]...` | Error. One line, codes only — no prose. |
| `? <warning> [arg]...` | Warning. Appears **before** the success envelope, one per line. |

A success response with warnings:

```
? nav https://example.com/login
? captcha_visible turnstile unrendered
@a3f9b2c1d4e6f70a
... body ...
<blank line>
```

A pure-error response:

```
! STALE_TOKEN 9c14e7df0a223f88 nav
<blank line>
```

### Error codes (initial set)

Documented in [`codes.md`](codes.md). Highlights:

| Code | Args | Meaning |
| ---- | ---- | ------- |
| `STALE_TOKEN` | `<new_token> <reason>` | Page changed since the read the write was based on. Reasons: `nav`, `mutate`, `expired`. |
| `ENGINE_UNSUPPORTED` | `<primitive> <engine>` | The active engine cannot service this primitive. |
| `POLICY_DENY` | `<rule> <subject>` | A policy rule blocked the action. |
| `TIMEOUT` | `<budget> <primitive>` | Operation exceeded its timeout. |
| `NOT_FOUND` | `ref=<n>` or `mark=<name>` | Target does not exist. |
| `CONFIRM_REQUIRED` | `<reason> [<detail>...]` | Action requires explicit confirmation. |

### Warning codes (initial set)

| Code | Args | Meaning |
| ---- | ---- | ------- |
| `nav` | `<new_url>` | Page navigated; refs reset and a fresh full tree follows. |
| `captcha_visible` | `<provider> <state>` | A bot challenge is present and unsolved. `<state>` is `pending` (widget up, human-completable) or `unrendered` (no widget produced). The matching tree node carries `challenge=<provider>:<state>`. |
| `auth_loaded` | `<name>` | An auth blob was applied; next view forces a re-baseline. |
| `viewport_changed` | `<W>x<H>` | Viewport changed; layout may have shifted. |

## State token

```
token = blake3( canonical_a11y_tree(page) || url(page) || page_id )[..8].hex()
```

- 16 hex characters, lowercase.
- Two structurally identical trees produce the same token. Whitespace-only
  changes do not bump the token (canonicalization sorts attributes and
  normalizes whitespace before hashing).
- Reads (`vs_view`, `vs_read`, `vs_extract`, `vs_layout`) return the current
  token in the success envelope.
- Writes (`vs_act`) require the token from the read they are based on. The
  wire form is implicit: the daemon associates `(agent, page)` with the
  last-emitted token. If an explicit token is needed for parallel agents,
  it is passed as `--token=<hex>` on the request line.
- Mismatch → `! STALE_TOKEN <new_token> <reason>`.

## Tree representation (`vs_view`, full)

Indentation is two spaces per nesting level. One element per line:

```
<ref> <role> <label> [op[,op]...] [k=v]...
```

- `<ref>` is a positive integer, monotonic per session.
- `<role>` is a short code from the role table in [`codes.md`](codes.md).
- `<label>` is bare-quoted if it contains whitespace: `"Sign in to continue"`.
- `op` lists the operations actually applicable to this ref. The full
  vocabulary: `click fill scroll key submit hover focus`.
- `k=v` attributes appear only when meaningful. Empty defaults are omitted.

Example:

```
@e8a1c0fdc7c1bcfb
1 doc "Example Domain"
  2 hd "Example Domain"
  3 p "This domain is for use in illustrative examples..."
  4 lnk "More information..." click href=https://www.iana.org/domains/example
```

## Tree deltas (default for `vs_view`)

After the first `vs_view` on a page, subsequent calls return only the diff
against the last-emitted tree for `(page_id, agent_id)`. Five operations:

| Op | Wire | Meaning |
| -- | ---- | ------- |
| Add     | `+<ref>@<parent>[:<pos>] <role> <label> [...]` | New node. |
| Remove  | `-<ref>` | Node and its subtree are gone. |
| Update  | `~<ref> <k>=<v> [...]`                         | Attributes changed. |
| Move    | `><ref>@<parent>[:<pos>]`                      | Reparented or reordered. |
| Replace | `*<ref>` then indented subtree                 | Subtree blown away; new tree follows. |

- `:<pos>` is optional. Omit when appending or when the parent is unordered.
- If a subtree's edit distance exceeds 40% of nodes (size > 5), emit a
  `Replace` instead of dozens of finer ops.
- If nothing changed since the last token for this `(page, agent)`, the
  body is empty — only the envelope and a blank line.

## Navigation, viewport, and auth events

These events reset the delta baseline and emit a warning before the fresh
full tree:

- Top-level navigation → `? nav <new_url>`
- Viewport change via `vs_viewport` → `? viewport_changed <W>x<H>`
- Auth load via `vs_auth load` → `? auth_loaded <name>`

After any of these, the next `vs_view` body is a fresh full tree as if it
were the first call on the page.

## Idempotency

Every `vs_act` is implicitly idempotent on `(page_id, before_token,
args_hash)` for 30 seconds. A repeat with the same args on the same
pre-image token returns the cached result without re-executing the action.
The audit row is still written and marked with `idempotency_hit=1`.

## Reads vs. writes (summary)

| Primitive | Token in | Token out |
| --------- | -------- | --------- |
| `vs_session_open`  | —        | yes       |
| `vs_session_close` | —        | —         |
| `vs_open <url>`    | —        | yes (page) |
| `vs_close [page]`  | —        | —         |
| `vs_view [page]`   | —        | yes        |
| `vs_read <ref>`    | optional | yes        |
| `vs_act <ref> <op> [val]` | required | yes |
| `vs_find <query>`  | —        | yes        |
| `vs_wait <cond>`   | —        | yes        |
| `vs_extract <schema>` | required | yes     |
| `vs_mark <ref> <name>` | required | —      |
| `vs_annotate <target> <key> <val>` | — | — |
| `vs_status`        | —        | —          |
| `vs_log [...]`     | —        | —          |
| `vs_skill <name>`  | —        | yes        |
| `vs_capture [ref]` | —        | yes        |
| `vs_download <page> [url]` | — | yes      |
| `vs_viewport <preset|WxH>` | — | yes (re-baseline) |
| `vs_layout <ref>...` | —      | yes        |
| `vs_auth <save|load|list|clear> [name]` | — | yes (load triggers re-baseline) |

See [`PRIMITIVES.md`](PRIMITIVES.md) for full per-primitive specs.

## Wait conditions

`vs_wait <cond> [val]` blocks until `<cond>` is met or the timeout fires.
Supported conditions:

| Condition | Argument | Meaning |
| --------- | -------- | ------- |
| `stable` | none | A11y tree stable for 250ms. |
| `net-idle` | none | Network idle (≤ 0 in-flight requests for 500ms). |
| `ref` | `<ref>` | Ref appears in the tree. |
| `gone` | `<ref>` | Ref disappears from the tree. |
| `text` | `<text>` | Text matches anywhere in the tree (exact substring). |
| `token-change` | none | `state_token` changes. |

Default timeout: 5000ms. Override per call with `--timeout=<ms>`. Daemon
caps at 60000ms.

## Versioning

`vs_status` opens with a daemon identity line:

```
daemon	version=<x.y.z>	proto=<n>	flags=<csv>
```

- `version` is the daemon's semver.
- `proto` is the wire-protocol level. `proto=1` is the frozen contract
  described here; the invariants below hold for any `proto=1` daemon.
- `flags` is a comma-separated list of negotiable capabilities a client
  may switch on when present. Current flags: `actDeltas` (writes return
  the post-write tree delta inline, see below) and `clickVia` (robotic
  ref-click fast path). Clients read this line instead of pinning a
  hardcoded daemon version.

Compatibility within `proto=1` is guaranteed: new daemons may add flags
and primitives, but the framing, token semantics, delta grammar, and the
invariants below do not change without a `proto` bump.

## Invariants (proto=1)

These hold for every `proto=1` daemon and clients may rely on them:

1. **Every returned token is a valid pre-image.** Any primitive that
   returns a `state_token` returns one you can pass as the `--token` of
   the very next `vs_act` on that page without a `STALE_TOKEN`, provided
   nothing else mutates the page in between. Writes (`vs_act`), cursor
   ops (`vs_click_at`, `vs_drag`), and `vs_wait` all advance the page's
   delta baseline to the tree behind the token they return.
2. **Writes return their own delta (`actDeltas`).** `vs_act` returns the
   post-action tree delta in the response body (same grammar as
   `vs_view`), plus the new token, so a client never needs a follow-up
   `vs_view` just to see what an action changed. An idempotent replay
   returns `? idempotent_hit` with an empty (`NoChange`) body.
3. **Ref 0 is reserved.** It is the delta grammar's root-level parent
   sentinel (`+<ref>@0`), so no real node is ever emitted with ref 0.
   Refs are positive integers, monotonic per session.
4. **`NoChange` is an empty body.** When a read finds the page identical
   to the last token handed out for `(page, agent)`, the body is empty —
   only the envelope. This is distinct from a full tree that happens to
   be small.

## Open questions deferred to later milestones

- Streaming progress for long primitives (`vs_wait`, downloads). Currently
  blocking; consider adding `: progress` lines under the warning sigil.
- Network-mode framing if the daemon is ever exposed beyond the local
  socket. Not in scope for v1.
- MCP shim translation table (v1.1).
