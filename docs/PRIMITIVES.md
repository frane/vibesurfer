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

## 1. `vs_session_open [--policy=<name>]`

Open a new session. Returns the session id and a fresh `state_token` for
the empty session.

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
