<h1 align="center">vibesurfer (<code>vs</code>)</h1>

<p align="center"><strong>A real browser for your local AI agent.</strong></p>

<p align="center">
  <a href="https://github.com/frane/vibesurfer/actions/workflows/ci.yml"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/frane/vibesurfer/ci.yml?branch=main&label=ci&style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/actions/workflows/engine-tests.yml"><img alt="engine-tests" src="https://img.shields.io/github/actions/workflow/status/frane/vibesurfer/engine-tests.yml?branch=main&label=engine-tests&style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/frane/vibesurfer?style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/blob/main/LICENSE"><img alt="license" src="https://img.shields.io/badge/license-Apache_2.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="docs/demo-claude.gif" alt="Claude Code using vibesurfer" width="560">
</p>

## Why

I wanted agents to automate browser work. First try Playwright, second Puppeteer directly. Both fell back to CDP underneath. CDP dropped sessions. Playwright crashed on long runs. Chrome got heavier every minor version. The bug wasn't any of those. CDP and Chrome were built for humans staring at DevTools, not for an LLM in a loop.

A human watching DevTools wants pixel-accurate rendering, async events as they fire, the full DOM on demand. An LLM pays per token, blocks per response, can't afford an event firehose or a 4000-token DOM dump on every read. One Hacker News front page rendered through Playwright is around 2000 input tokens before the agent has done any actual work. The same flow through vibesurfer is around 50.

So vibesurfer. A native browser daemon in Rust with a wire designed for LLM callers. Reads return state tokens and tree deltas instead of full DOM. Writes check the token. Three real engines underneath (WKWebView on macOS, WebKitGTK on Linux, WebView2 on Windows); the protocol on top is small, synchronous, line-oriented.

For tasks that need pixels, `vs capture` takes a screenshot, `vs viewport` switches between mobile and desktop layouts, and `vs layout` returns bounding boxes. The default path is text in, structured deltas out.

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

## Wire it into your agent

The fastest path is the binary's auto-installer. After `vs` is on your PATH:

```
vs skill install
```

It detects Claude Desktop, Claude Code, Cursor, Codex CLI, Gemini CLI, and OpenClaw on your machine and writes the SKILL.md plus an MCP entry into each. Re-run after upgrading.

If you'd rather install per-agent, the manifests below are in the repo and supported by each tool's native plugin command:

### Claude Code

```
/plugin install frane/vibesurfer
```

Resolves `.claude-plugin/marketplace.json` at the repo root and `plugin/.claude-plugin/plugin.json`. The plugin registers the MCP server (`vs mcp`) and ships the SKILL.md so Claude knows when to reach for it.

### Codex CLI (OpenAI)

The plugin manifest at `plugin/.codex-plugin/plugin.json` declares the skills directory and the MCP entry. Install via Codex's plugin command, or do it by hand:

```
mkdir -p ~/.codex/skills && cp -r plugin/skills/vibesurfer ~/.codex/skills/
```

Then add the MCP server to `~/.codex/config.toml`:

```toml
[mcp_servers.vibesurfer]
command = "vs"
args = ["mcp"]
```

`vs skill install` does both steps automatically.

### Gemini CLI

```
gemini extensions install https://github.com/frane/vibesurfer
```

Reads `gemini-extension.json` at the repo root, which wires `vs mcp` as the server and points at `plugin/GEMINI.md` for the context file.

### Claude Desktop or any MCP-aware agent

Edit the tool's MCP config (`claude_desktop_config.json`, Cursor's `mcp.json`, etc.) and drop in:

```json
{
  "mcpServers": {
    "vibesurfer": {
      "command": "vs",
      "args": ["mcp"]
    }
  }
}
```

The same JSON sits at `plugin/.mcp.json` if you want to copy from the repo.

## Short forms

Every primitive has a one-to-three-letter alias. Long forms exist for documentation; agent invocations should use the short form to save tokens.

| Long           | Short | | Long       | Short |
|----------------|-------|-|------------|-------|
| `session-open` | `so`  | | `extract`  | `x`   |
| `session-close`| `sc`  | | `mark`     | `m`   |
| `open`         | `o`   | | `annotate` | `an`  |
| `close`        | `c`   | | `status`   | `st`  |
| `view`         | `v`   | | `log`      | `l`   |
| `read`         | `r`   | | `skill`    | `sk`  |
| `act`          | `a`   | | `capture`  | `cap` |
| `find`         | `f`   | | `viewport` | `vp`  |
| `wait`         | `w`   | | `layout`   | `lay` |
| `auth`         | `au`  | | `inspect`  | `i`   |

Frequent flags: `--session=` / `-S`, `--full` / `-F`, `--since=` / `-s`, `--limit=` / `-n`, `--page=` / `-P`, `--json` / `-j`. Inspect subcommands have one-or-two-letter aliases too (`i co` for `inspect console`, `i n` for `network`, `i req` for `request`, `i e` for `eval`, `i s` for `storage`, `i scr` for `scripts`, `i src` for `script`, `i d` for `dom`, `i p` for `performance`).

Both forms work everywhere. The integration tests assert that the wire request from a short form is byte-identical to the wire request from the long form.

## Quickstart

```
$ vs so                                        # session-open
@0                                             # state token (16 hex chars; 0 means none yet)
s_019e08a7…                                    # session id

$ vs o https://example.com                     # open the URL
@0                                             # the open call doesn't carry a snapshot
p_019e08a7…                                    # page id

$ vs v p_019e08a7…                             # view (snapshot the a11y tree)
@44d01704049d6d31                              # state token
1 doc "Example Domain"                         # ref 1, document
  0 el ""                                      # nameless wrapper
    2 hd "Example Domain"                      # ref 2, heading
    3 p  "This domain is for use in…"          # ref 3, paragraph
    5 p  "Learn more"                          # ref 5, paragraph
      4 lnk "Learn more" click,focus           # ref 4, link, supported ops
```

A snapshot is a list of refs. Each ref is an integer that survives across snapshots, so the agent can act on ref 4 ten turns later without re-reading the whole page. The two-letter codes (`hd`, `p`, `lnk`, `btn`, `tf`, …) compress the role into a few bytes instead of an ARIA string. Labels are in quotes; the trailing tokens after a label list which `vs act` operations the element supports. About twenty role codes total, listed in [docs/PROTOCOL.md](docs/PROTOCOL.md).

```
$ vs a 4 click                                 # act: click ref 4
@<new-token>                                   # new token, page mutated
?nav                                           # warning: navigation occurred
… new tree …                                   # the act response carries deltas;
                                               # on navigation it re-baselines to a full tree
```

`vs act` is the only mutating primitive. It takes a ref and an operation (`click`, `fill`, `scroll`, `key`, `submit`, `hover`, `focus`) and requires the most recent state token. If the page mutated between read and write (a JS timer fired, a websocket pushed an update, anything), the call returns `! STALE_TOKEN` and the agent re-reads. No silent stale clicks. After a successful act on the same page (no navigation), the response carries only the deltas (`+ref` for adds, `-ref` for removes, `~ref` for attribute changes), so a click that adds one button costs ~20 bytes on the wire instead of the whole DOM.

```
$ vs st                                        # status
session  s_019e08a7…  pages=1
page     p_019e08a7…  url=https://www.iana.org/help/example-domains  token=…
```

Every primitive call writes one row to a SQLite audit log before it returns. `vs status` reads that log. So does `vs log`. Replay, debugging, and governance all collapse to SQL queries against `~/.vibesurfer/state.db`. There is no separate event stream to subscribe to.

The daemon auto-spawns on first call. State, captures, and downloads live under `~/.vibesurfer/`. The transport is an AF_UNIX socket on Unix (`~/.vibesurfer/daemon.sock`) and a Windows named pipe on Windows; either way, the CLI handles the difference.

20 primitives total, each documented in [docs/PRIMITIVES.md](docs/PRIMITIVES.md). The full wire format with every sigil and edge case is in [docs/PROTOCOL.md](docs/PROTOCOL.md). The per-platform per-primitive verification matrix is in [docs/REALITY_CHECK.md](docs/REALITY_CHECK.md).

## Configuration

| Path / variable | Purpose |
|---|---|
| `~/.vibesurfer/state.db` | SQLite, holds sessions, audit, marks, auth blobs |
| `~/.vibesurfer/daemon.sock` *(Unix)* | AF_UNIX socket the CLI talks to |
| Windows named pipe | Same role on Windows; resolved automatically |
| `~/.vibesurfer/captures/` | Screenshots from `vs capture` |
| `VS_CAPTURES_DIR` | Override the capture directory |
| `VS_HOME` | Override the vibesurfer home directory |
| `VS_DISABLE_INSPECTOR=1` | Skip inspector hooks (testing only) |
| `VS_DAEMON_BIN` | Override the binary used for daemon auto-spawn (tests) |

## Build from source

Requires Rust 1.85+. Platform-specific dependencies:

- **macOS** (15+): nothing extra, links against system WebKit.
- **Linux**: `libwebkitgtk-6.0-dev`, `libgtk-4-dev`, `libsoup-3.0-dev`.
- **Windows**: WebView2 SDK pulled by `webview2-com` at build time; the WebView2 Runtime is required at run time.

```
git clone https://github.com/frane/vibesurfer && cd vibesurfer
cargo build --release
```

Run the test suite:

```
cargo test --workspace --lib --bins        # fast unit tests
cargo test --workspace                     # adds integration tests (real engine)
```

For Linux engine tests on a non-Linux host, use the Docker container. WebKitGTK 6's sandbox needs unprivileged user namespaces; the CI Linux job relaxes the AppArmor restriction with one sysctl on the bare runner, while the Docker fallback needs `--privileged` to do the same:

```
docker build -f Dockerfile.linux-test -t vs-test-linux .
docker run --rm --privileged -v "$PWD":/work vs-test-linux
```

See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) for the longer walkthrough.

The demo gif at the top of this README is a real interactive Claude Code session driving vibesurfer. To capture a fresh one, run `docs/demo/record-claude.sh`:

```
brew install asciinema agg
docs/demo/record-claude.sh         # writes docs/demo-claude.gif
```

The script enforces a TTY guard, isolates the demo home, and locks Claude to Bash so the agent must use the real `vs` binary (no MCP fallback, no built-in file tools). Each render is non-deterministic, since model output varies. The cached gif is committed so cloners and CI don't re-render.

## Contributing

Issues and pull requests welcome. Open an issue first for anything beyond a small fix so we can discuss the approach. The codebase uses [agented](https://github.com/frane/agented) for transactional file edits during development; agented's workspace state is local-only (`.agented/state.db`) and is not committed.

## Acknowledgments

Built on:

- [objc2](https://github.com/madsmtm/objc2), macOS WebKit FFI.
- [webkit6](https://github.com/gtk-rs/gtk-rs-core), Linux engine bindings.
- [webview2-com](https://github.com/wravery/webview2-rs), Windows COM layer.
- [interprocess](https://github.com/kotauskas/interprocess), cross-platform IPC transport.
- [tiny_http](https://github.com/tiny-http/tiny-http), integration test fixture server.

Protocol borrows from [agented](https://github.com/frane/agented), an editor for AI agents.

## License

Apache-2.0. See [LICENSE](LICENSE).
