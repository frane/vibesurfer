# vibesurfer

A real browser for your local AI agent.

![demo](docs/demo.gif)

[![CI](https://github.com/frane/vibesurfer/actions/workflows/ci.yml/badge.svg)](https://github.com/frane/vibesurfer/actions/workflows/ci.yml)
[![M6](https://github.com/frane/vibesurfer/actions/workflows/m6.yml/badge.svg)](https://github.com/frane/vibesurfer/actions/workflows/m6.yml)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)

## What is this

I wanted agents to automate browser work for me. The first try used Playwright, the second tried Puppeteer directly, both eventually fell back to the Chrome DevTools Protocol underneath. All three were unreliable in different ways. CDP would drop sessions. Playwright would crash on long runs. Chrome itself would get heavier every minor version. Every fix I tried was patching around a deeper problem, which is that CDP and Chrome were both built for humans, not for an LLM in a loop.

A human watching DevTools wants pixel-accurate rendering, async events as they happen, and the full DOM available on demand. An LLM wants the opposite. It pays for every token of context, blocks on every response, and cannot afford an event firehose or a 4000-token DOM dump on every read. Optimizing the same engine for both audiences is a lost cause.

So I wrote vibesurfer. A native browser daemon in Rust with a protocol designed for LLM callers from the first commit. Reads return state tokens and tree deltas instead of full DOM. Writes check the token, fail fast on stale state, and idempotent retries are free. Three real engines underneath (WKWebView on macOS, WebKitGTK on Linux, WebView2 on Windows) handle the actual web rendering, but the protocol on top is small, synchronous, and line-oriented.

Visual rendering still works. `vs capture` takes a screenshot, `vs viewport` switches between mobile and desktop layouts, `vs layout` returns bounding boxes. But those are escape hatches for tasks that genuinely need pixels. The default path is text in, structured deltas out, optimal for the agent loop.

## Status

vibesurfer is in beta. The protocol is stable, the macOS and Linux backends are verified by 48 cells of integration tests against real fixture pages, and the Windows backend ships the same tests on `windows-latest` CI as `pending-manual-verification` until the maintainer signs off on a green run. Real sites work, but coverage is not exhaustive. If you hit something broken, please open an issue with the URL and the steps.

See [docs/REALITY_CHECK.md](docs/REALITY_CHECK.md) for the per-platform per-primitive verification matrix.

## Install

Homebrew (macOS, Linux):

```
brew tap frane/tap && brew install vibesurfer
```

curl:

```
curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh | sh
```

From source:

```
git clone https://github.com/frane/vibesurfer && cd vibesurfer
cargo install --path crates/vs-cli
```

Linux needs WebKitGTK 6. Windows needs the WebView2 runtime (already on Windows 11, available for Windows 10 from Microsoft).

## Quickstart

```
vs skill install
```

That command detects Claude Desktop, Cursor, Codex, Gemini, and OpenClaw on your machine and registers vibesurfer as a tool in each. Open any of them, ask the agent to do something on the web, and your agent can browse.

The daemon auto-spawns on first call. State, captures, and downloads live under `~/.vibesurfer/`. The transport is an AF_UNIX socket on Unix (`~/.vibesurfer/daemon.sock`) and a Windows named pipe on Windows; either way, the CLI handles the difference.

## Using vibesurfer directly

For debugging, scripting, or building your own integration, talk to the daemon through the `vs` CLI. Here is what the wire actually looks like, with annotations.

```
$ vs session-open                              # start a new session
@0                                             # state token (16 hex chars; 0 means none yet)
s_019e08a7…                                    # session id

$ vs open https://example.com                  # open the URL
@0                                             # the open call doesn't carry a snapshot
p_019e08a7…                                    # page id

$ vs view p_019e08a7…                          # snapshot the a11y tree
@44d01704049d6d31                              # state token
1 doc "Example Domain"                         # ref 1, document
  0 el ""                                      # nameless wrapper
    2 hd "Example Domain"                      # ref 2, heading
    3 p  "This domain is for use in…"          # ref 3, paragraph
    5 p  "Learn more"                          # ref 5, paragraph
      4 lnk "Learn more" click,focus           # ref 4, link, supported ops
```

A snapshot is a list of refs. Each ref is an integer that survives across snapshots, which means the agent can act on ref 4 ten turns later without re-reading the whole page. The two-letter codes (`hd`, `p`, `lnk`, `btn`, `tf`, …) compress the role into a few bytes instead of an ARIA string. Labels are in quotes; the trailing tokens after a label list which `vs act` operations the element supports. About twenty role codes total, listed in [docs/PROTOCOL.md](docs/PROTOCOL.md).

```
$ vs act 4 click                               # click ref 4
@<new-token>                                   # new token, page mutated
?nav                                           # warning: navigation occurred
… new tree …                                   # the act response carries deltas; on
                                               # navigation it re-baselines to a full tree
```

`vs act` is the only mutating primitive. It takes a ref and an operation (`click`, `fill`, `scroll`, `key`, `submit`, `hover`, `focus`). Behind the scenes it requires the most recent state token. If the page mutated between read and write (a JS timer fired, a websocket pushed an update, anything), the call returns `! STALE_TOKEN` and the agent re-reads. This is what stops silent stale clicks. After a successful act on the same page (no navigation), the response gives back only the deltas (`+ref` for adds, `-ref` for removes, `~ref` for attribute changes), so a click that adds one button to the page costs about 20 bytes on the wire instead of the whole DOM.

```
$ vs status                                    # session summary
session  s_019e08a7…  pages=1
page     p_019e08a7…  url=https://www.iana.org/help/example-domains  token=…
```

Every primitive call writes one row to a SQLite audit log before it returns. `vs status` reads that log. So does `vs log`. Replay, debugging, and governance all collapse to SQL queries against `~/.vibesurfer/state.db`. There is no separate event stream to subscribe to.

20 primitives total, each documented in [docs/PRIMITIVES.md](docs/PRIMITIVES.md). The full wire format with every sigil and edge case is in [docs/PROTOCOL.md](docs/PROTOCOL.md).

## Configuration

Most users don't need to configure anything. The daemon picks sensible defaults.

| Path / variable | Purpose |
|---|---|
| `~/.vibesurfer/state.db` | SQLite — sessions, audit, marks, auth blobs |
| `~/.vibesurfer/daemon.sock` *(Unix)* | AF_UNIX socket the CLI talks to |
| Windows named pipe | Same role on Windows; resolved automatically |
| `~/.vibesurfer/captures/` | Screenshots from `vs capture` |
| `VS_CAPTURES_DIR` | Override the capture directory |
| `VS_HOME` | Override the vibesurfer home directory |
| `VS_DISABLE_INSPECTOR=1` | Skip inspector hooks (testing only) |
| `VS_DAEMON_BIN` | Override the binary used for daemon auto-spawn (tests) |

## Build from source

Requires Rust 1.85+. Platform-specific dependencies:

- **macOS** (15+): nothing extra, links against system WebKit
- **Linux**: `libwebkitgtk-6.0-dev`, `libgtk-4-dev`, `libsoup-3.0-dev`
- **Windows**: WebView2 SDK pulled by `webview2-com` at build time; the WebView2 Runtime is required at run time

```
git clone https://github.com/frane/vibesurfer && cd vibesurfer
cargo build --release
```

Run the test suite:

```
cargo test --workspace --lib --bins        # fast unit tests
cargo test --workspace                     # adds integration tests (real engine)
```

For Linux engine tests on a non-Linux host, use the Docker container. WebKitGTK 6's sandbox needs unprivileged user namespaces, which the bare GitHub runners restrict; the M6 Linux job in CI runs the same container with `--privileged`:

```
docker build -f Dockerfile.linux-test -t vs-test-linux .
docker run --rm --privileged -v "$PWD":/work vs-test-linux
```

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the longer walkthrough.

The demo gif at the top of this README is a scripted walk-through of the `vs` CLI. To capture a real interactive Claude Code session driving vibesurfer instead, run `docs/demo/record-claude.sh`:

```
brew install asciinema agg
docs/demo/record-claude.sh         # writes docs/demo-claude.gif
```

The script enforces a TTY guard, isolates the demo home, and locks Claude to Bash so the agent must use the real `vs` binary (no MCP fallback, no built-in file tools). Each render is non-deterministic — model output varies. The cached gif is committed so cloners and CI don't re-render.

Or regenerate the scripted README gif with [vhs](https://github.com/charmbracelet/vhs):

```
brew install vhs
docs/demo/render.sh                # writes docs/demo.gif
```

## Contributing

Issues and pull requests welcome. Open an issue first for anything beyond a small fix so we can discuss the approach. The codebase uses [agented](https://github.com/frane/agented) for transactional file edits during development; agented's workspace state is local-only (`.agented/state.db`) and is not committed.

## Acknowledgments

vibesurfer is built on the work of several excellent projects:

- [objc2](https://github.com/madsmtm/objc2) for the macOS WebKit FFI
- [webkit6](https://github.com/gtk-rs/gtk-rs-core) for the Linux engine bindings
- [webview2-com](https://github.com/wravery/webview2-rs) for the Windows COM layer
- [interprocess](https://github.com/kotauskas/interprocess) for the cross-platform IPC transport
- [tiny_http](https://github.com/tiny-http/tiny-http) for the integration test fixture server

The shape of the protocol owes a lot to thinking about [agented](https://github.com/frane/agented), an editor designed for AI agents.

## License

MIT. See [LICENSE](LICENSE).
