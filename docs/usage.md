# Using vibesurfer

The full command reference an agent reads is [SKILL.md](../skills/vibesurfer/SKILL.md). The wire format, with every sigil and edge case, is in [PROTOCOL.md](PROTOCOL.md).

## A first session

```
$ vs so                                        # session-open
@0                                             # state token (0: none yet)
s_019e08a7…                                    # session id

$ vs o https://example.com                     # open
@0
p_019e08a7…                                    # page id

$ vs v p_019e08a7…                             # view the accessibility tree
@44d01704049d6d31                              # state token
1 doc "Example Domain"
  0 el ""
    2 hd "Example Domain"
    3 p  "This domain is for use in…"
    5 p  "Learn more"
      4 lnk "Learn more" click,focus
```

A snapshot is a list of refs. A ref is an integer that survives across snapshots, so the agent can act on ref 4 ten turns later without reading the page again. The two-letter codes (`hd`, `p`, `lnk`, `btn`, `tf`, ...) are roles; the tokens after a label are the operations the element supports. The role codes are listed in [PROTOCOL.md](PROTOCOL.md).

```
$ vs a p_019e08a7… 4 click --token=44d01704049d6d31
@<new-token>
?nav                                           # a navigation happened
… new tree …
```

`vs act` takes a ref, an operation (`click`, `fill`, `scroll`, `key`, `submit`, `hover`, `focus`) and the latest state token. If the page changed between the read and the write, it returns `! STALE_TOKEN` and the agent reads again instead of clicking a stale page. Without a navigation, the response carries only deltas (`+ref` added, `-ref` removed, `~ref` changed), so a click that adds one button costs about 20 bytes.

Every call writes one row to a SQLite audit log before it returns. `vs status` and `vs log` read it; replay and debugging are queries against `~/.vibesurfer/state.db`.

The daemon starts on the first call. The CLI talks to it over a Unix socket (`~/.vibesurfer/daemon.sock`) or a named pipe on Windows.

## Short forms

Every command has a short alias, and agents should use it.

| Long | Short | | Long | Short |
|---|---|-|---|---|
| `session-open` | `so` | | `extract` | `x` |
| `session-close` | `sc` | | `mark` | `m` |
| `open` | `o` | | `annotate` | `an` |
| `goto` | `g` | | `status` | `st` |
| `close` | `c` | | `log` | `l` |
| `view` | `v` | | `skill` | `sk` |
| `read` | `r` | | `capture` | `cap` |
| `act` | `a` | | `download` | `dl` |
| `find` | `f` | | `viewport` | `vp` |
| `wait` | `w` | | `layout` | `lay` |
| `auth` | `au` | | `inspect` | `i` |

Flags: `-S` (`--session`), `-F` (`--full`), `-s` (`--since`), `-n` (`--limit`), `-P` (`--page`), `-j` (`--json`). Inspect kinds: `i co` console, `i n` network, `i req` request, `i e` eval, `i s` storage, `i scr` scripts, `i src` script, `i res` response, `i d` dom, `i p` performance. A short form sends the same wire request as its long form; the tests check that byte for byte.

## For the humans next to the agent

- `vs prompt-form` asks for credentials through a form on a single-use `127.0.0.1` link. The password manager fills it, the daemon writes the values into the page, and the agent never sees them.
- `vs watch` prints a link to a read-only live view of a page (about 1 fps). In MCP Apps hosts (Claude Desktop, ChatGPT) it shows as a panel in the conversation.
- `vs record start` / `stop` saves the page as an H.264 MP4 or VP8 WebM. It captures a frame at every mouse move, keystroke and click and draws the cursor in, so the video moves continuously. No ffmpeg needed.

## Configuration

| Path or variable | Purpose |
|---|---|
| `~/.vibesurfer/state.db` | SQLite: sessions, audit log, marks, auth blobs |
| `~/.vibesurfer/daemon.sock` | The socket the CLI talks to (a named pipe on Windows) |
| `~/.vibesurfer/captures/` | Screenshots and recordings |
| `~/.vibesurfer/downloads/` | Files saved by `vs download` |
| `--home DIR` | Use another vibesurfer home (its own daemon, store and socket) |
| `VS_CAPTURES_DIR`, `VS_DOWNLOADS_DIR` | Move those two directories |
| `VS_SESSION` | Pin the session id (recommended in scripts; see [known-issues.md](known-issues.md)) |
| `VS_CALLER` | A stable caller name: the same name gets the same session across restarts |
| `VS_THUMBS=1` | On `vs mcp`: attach a thumbnail to every act and open result |
| `VS_NAV_BUDGET_MS` | How long `open` and `goto` wait for a navigation (default 15000) |
| `VS_PAGE_IDLE_SECS`, `VS_SESSION_TTL_SECS`, `VS_REAP=0` | When idle pages go dormant (1800) and idle sessions close (86400); `0` turns reaping off |
| `VS_DISABLE_INSPECTOR=1`, `VS_DAEMON_BIN` | Testing only |
