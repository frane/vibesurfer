<h1 align="center">vibesurfer (<code>vs</code>)</h1>

<p align="center"><strong>A browser for LLMs, not humans.</strong></p>

<p align="center">
  <a href="https://github.com/frane/vibesurfer/actions/workflows/ci.yml"><img alt="ci" src="https://img.shields.io/github/actions/workflow/status/frane/vibesurfer/ci.yml?branch=main&label=ci&style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/actions/workflows/engine-tests.yml"><img alt="engine-tests" src="https://img.shields.io/github/actions/workflow/status/frane/vibesurfer/engine-tests.yml?branch=main&label=engine-tests&style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/releases/latest"><img alt="release" src="https://img.shields.io/github/v/release/frane/vibesurfer?style=flat-square"></a>
  <a href="https://www.npmjs.com/package/vibesurfer"><img alt="npm" src="https://img.shields.io/npm/v/vibesurfer?style=flat-square&label=npm"></a>
  <a href="https://crates.io/crates/vibesurfer"><img alt="crates.io" src="https://img.shields.io/crates/v/vibesurfer?style=flat-square"></a>
  <a href="https://github.com/frane/vibesurfer/blob/main/LICENSE"><img alt="license" src="https://img.shields.io/badge/license-Apache_2.0-blue?style=flat-square"></a>
</p>

<p align="center">
  <img src="https://github.com/frane/vibesurfer/raw/main/docs/demo-claude.gif" alt="Claude Code using vibesurfer" width="560">
</p>

## Why

I wanted agents to test web apps via the browser. Everything I tried (Playwright, Puppeteer, anything else that wraps CDP) was too heavy and too unstable. CDP drops sessions. Playwright crashes on long runs. Chrome gets fatter every release. None of that is the actual problem though. CDP and Chrome were designed for humans staring at DevTools. They were never designed for an agent stuck in a while loop.

An agent pays per token. It blocks per response. It can't deal with the event firehose, and a 4kb DOM dump on every read burns the context budget fast. The Hacker News front page through Playwright is about 2000 input tokens before the agent has done anything. Through vibesurfer it's around 50.

vibesurfer is a native browser daemon in Rust. Reads return state tokens and tree deltas instead of the full DOM. Writes check the token. If anything moved between the read and the write, the call fails and the agent re-reads instead of clicking on a stale page. There are three real engines underneath: WKWebView on macOS, WebKitGTK on Linux, WebView2 on Windows. The protocol on top is text and line-oriented.

When you actually need pixels there's `vs capture` for screenshots, `vs viewport` to switch between mobile and desktop layouts, and `vs layout` to get bounding boxes. But text comes first.

When you need a file rather than a page, `vs download` saves one to disk. Given a URL it reads it from inside the page, so a session cookie still applies and a PDF behind a login comes down. Given no URL it hands back whatever the page last tried to save itself — a download link, a viewer's Save button — which a headless web view otherwise drops on the floor, since there is no download UI for it to go to.

## Install

Try it instantly, no install. `npx` downloads the prebuilt binary for your platform (checksum-verified, then cached), so this just works:

```
npx vibesurfer session-open
npx vibesurfer open https://example.com
```

Homebrew (macOS, Linux):

```
brew tap frane/tap && brew install vibesurfer
```

curl (macOS, Linux):

```
curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh | sh
```

PowerShell (Windows):

```
irm https://raw.githubusercontent.com/frane/vibesurfer/main/install.ps1 | iex
```

Cargo:

```
cargo install vibesurfer
```

From source:

```
git clone https://github.com/frane/vibesurfer && cd vibesurfer
cargo install --path crates/vs-cli
```

Linux needs WebKitGTK 6. Windows needs the WebView2 runtime (already on Windows 11, available for Windows 10 from Microsoft).

## Wire it into your agent

Two integration paths, and they're independent. You can install either or both:

- **Skill**: drop `SKILL.md` into the agent's skills directory. The agent reads it as context and calls the `vs` binary directly through whatever shell it has. Use this for any agent that runs Bash but doesn't speak MCP.
- **MCP**: register `vs mcp` as an MCP server. The agent calls vibesurfer primitives as MCP tools over JSON-RPC, no shell required. Use this for agents with native MCP support.

The auto-installer does both where supported. After `vs` is on your PATH:

```
vs skill install
```

It detects Claude Desktop, Claude Code, Cursor, Codex CLI, Google Antigravity, and OpenClaw, then writes the SKILL.md plus the MCP entry into each one. Agents that only support one of the two get only the relevant piece. Re-run after upgrading.

### Doing it by hand

For the **skill path**, copy `skills/vibesurfer/SKILL.md` from the repo into the agent's skills directory. For Claude-family agents that's typically `~/.claude/skills/vibesurfer/SKILL.md`.

For the **MCP path**, add this block to the agent's MCP config (`claude_desktop_config.json`, `.cursor/mcp.json`, etc.):

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

Codex uses TOML with the same shape under `[mcp_servers.vibesurfer]`. The JSON form also sits at `plugin/.mcp.json` if you would rather copy it from the repo.

### Per-agent shortcuts

**Claude Code marketplace** installs both surfaces from one command:

```
/plugin install frane/vibesurfer
```

Resolves `.claude-plugin/marketplace.json` at the repo root and `plugin/.claude-plugin/plugin.json`.


## Not detected as automated

Modern anti-bot systems (Cloudflare, hCaptcha, reCAPTCHA, DataDome) gate first on `event.isTrusted`, then on movement timing, then on TLS/HTTP fingerprinting. vibesurfer's input dispatch is built to pass all three.

On macOS, `vs act click` and the coordinate primitives (`vs click-at`, `vs hover-at`, `vs move-to`, `vs drag`) route through native `NSEvent` mouseDown / mouseUp / mouseMoved on `WKWebView`. Each event carries `isTrusted = true` in JS, same as a real cursor click. The Bezier-pathed lead-in dispatched before every click reproduces Fitts-law arrival timing (digraph-derived control points, optional overshoot) so the visible motion looks like a human reaching the target rather than a teleport.

Since v0.1.11 the coordinate primitives are native on Linux and Windows too: XTest over `x11rb` on WebKitGTK (X11 / Xwayland; pure Wayland falls back to `ENGINE_UNSUPPORTED`), `SendMouseInput` on a WebView2 composition controller on Windows. All three engines emit `isTrusted = true` for cursor-primitive clicks. Ref-based `vs act click` is trusted on macOS only — on Linux and Windows it still dispatches through injected JS (`isTrusted = false`); use the coordinate primitives there for fingerprint-sensitive sites.

Keyboard input has the same trust story: `vs type <TEXT>` sends native `NSEvent` KeyDown/KeyUp into the focused element (`isTrusted = true`, full keydown → beforeinput → input pipeline), so rich-text editors like DraftJS and ProseMirror — which ignore the programmatic `act fill` path — accept it. macOS only for now; Linux and Windows keyboard dispatch is the next step (they return `ENGINE_UNSUPPORTED`, and `act fill` covers plain inputs there).

The walker also honors ARIA `role="..."` (Radix UI, Headless UI, Reach UI, every custom-div-as-button pattern), plus a tabindex heuristic for focusable divs/spans without a role. Modern React UIs surface as actionable refs without coordinate workarounds.

### Where it actually stands

Run by vibesurfer against each site, macOS / WKWebView, v0.2.2. These are our own numbers, not a comparison against another browser.

| Probe | Result |
|---|---|
| [bot.sannysoft.com](https://bot.sannysoft.com/) | Every headless-artifact check passes — WebDriver, plugin array, languages, the PHANTOM_* and HEADCHR_* families, Selenium markers. `window.chrome` reports missing, which is correct for a WebKit browser and not a defect. |
| [bot.incolumitas.com](https://bot.incolumitas.com/) | 31 checks OK, 1 FAIL (`webDriverAdvanced`). `navigator.webdriver` is `false` behind a native getter on `Navigator.prototype`, with no own-property override and absent from `Object.keys` — spec-correct; the check expects a Chrome-shaped descriptor. |
| [CreepJS](https://abrahamjuliot.github.io/creepjs/) | Stable fingerprint, no lies section rendered. Headless heuristics: `chromium: false 6%`, `like headless: 33%`, `headless: 20%`. Evaluating its heuristic set directly, every flag that fires is a Chromium-only API WebKit does not have — `window.chrome`, Content Index, Contacts Picker, Network Information. The ones that would indicate a real problem (`noPlugins`, `noWebShare`, `hasWebDriver`, `noUserActivation`, `headlessUA`, the permissions bug) are all clear. `enumerateDevices` reports mic and webcam. |
| In-repo `fingerprint` cell | Passes on all three engines, every commit. Asserts no automation artifacts (`Function.prototype.toString` native for every builtin we replace, zero enumerable `__vs*` globals) and no impossible-browser values (`hasFocus`, non-zero `outerWidth`/`outerHeight`, visible document, sane screen/DPR/concurrency). |

Two of those artifacts were ours and shipped in 0.2.1: the download shim replaced `window.open`, `HTMLAnchorElement.prototype.click` and `URL.createObjectURL` so they stopped reporting `[native code]`, and every shim global sat enumerable on `window`. Both closed in 0.2.2 on all three engines, and the cell exists so they cannot come back.

The residual score on each of these is Chromium-shaped expectation, not defect. `window.chrome`, `navigator.connection`, the Contacts and Content Index APIs and a `webdriver` property that reads as absent are all things Chrome has and Safari does not — a real Safari scores the same way. We do not add them. Inventing a Chromium API in a browser whose User-Agent says Safari is precisely the internal inconsistency CreepJS's lie detection looks for, so faking them would raise our detectability, not lower it.

What the numbers do not mean: these are heuristic scores that move when the sites update, and passing them is not the same as passing a commercial anti-bot service on a live site. Cloudflare's own supported-browsers documentation places embedded engines under limited support regardless of configuration. Where a challenge does appear, the practical answer is to solve it rather than to be invisible — see the challenge handling in `SKILL.md`.

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
| `download`     | `dl`  | |            |       |
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

34 wire primitives total — the 19 core primitives are specified in [docs/PRIMITIVES.md](docs/PRIMITIVES.md); the later additions (`vs_inspect`, the four cursor primitives, trusted `vs_type`, prompt-input and the pending queue, prompt-form, watch, and record) are documented in the bundled [SKILL.md](crates/vs-cli/SKILL.md) and the CHANGELOG.

Two of those exist for the humans next to the agents. `vs prompt-form` asks for credentials through a browser form on a single-use `127.0.0.1` link — the human's password manager fills it, the daemon writes the values into the page, and the agent never sees them. `vs watch` prints the same kind of link for a read-only live view of a page (~1 fps) so you can watch what the agent's browser is doing; in MCP Apps hosts (Claude Desktop, ChatGPT) the view also renders as a panel inside the conversation. `vs record start`/`stop` saves the session to an H.264 MP4 while the agent works: it captures a frame at every mouse move, keystroke, and click and composites the cursor on (a headless snapshot has no OS pointer), so the video shows continuous motion rather than a slideshow. Encoding is real-time via openh264 and the file plays natively everywhere, no ffmpeg. The full wire format with every sigil and edge case is in [docs/PROTOCOL.md](docs/PROTOCOL.md). The per-platform per-primitive verification matrix is in [docs/REALITY_CHECK.md](docs/REALITY_CHECK.md).

## Configuration

| Path / variable | Purpose |
|---|---|
| `~/.vibesurfer/state.db` | SQLite, holds sessions, audit, marks, auth blobs |
| `~/.vibesurfer/daemon.sock` *(Unix)* | AF_UNIX socket the CLI talks to |
| Windows named pipe | Same role on Windows; resolved automatically |
| `~/.vibesurfer/captures/` | Screenshots (`vs capture`) and recordings (`vs record`) |
| `~/.vibesurfer/downloads/` | Files saved by `vs download` |
| `VS_CAPTURES_DIR` | Override the capture directory |
| `VS_DOWNLOADS_DIR` | Override the download directory |
| `VS_SESSION` | Pin the session id (recommended in scripts; see `docs/known-issues.md`) |
| `VS_CALLER` | Durable caller name; same name rebinds to the same session across restarts |
| `VS_THUMBS=1` | On `vs mcp`: attach a screenshot thumbnail to every act/open result |
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
