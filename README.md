# vibesurfer (`vs`)

A browser for LLMs, not humans.

Take a real WebKit, strip the chrome, expose it through a line-oriented Unix-socket protocol that treats token cost, state freshness, and audit trails as first-class concerns, and stop pretending an agent wants the same tooling a person staring at DevTools wants. What a browser optimises for changes when the user is the model: stable refs across snapshots, deltas instead of full re-reads, idempotent action replay, an audit row per call, an a11y tree typed with role codes that compress to two or three letters on the wire. CDP and Playwright started from "drive a browser the way a human attaches DevTools" and never quite recovered. vibesurfer starts from the agent.

## What users say...

> ⏺ vs view returns about a tenth as many tokens as Playwright's accessibility snapshot for the same page, and the tree still has everything I'd actually click.

*— Claude Code*

> • The state-token rejection on a stale write is what I want from every tool I use. One round trip on success, one round trip on conflict with the new content attached. No "did the page change since last view" guessing.

*— Codex CLI*

## Features

- **One real browser per backend.** Real WKWebView on macOS, real WebKitGTK 6 on Linux, real WebView2 on Windows. No headless-Chromium-via-CDP layer in between.
- **State tokens, not retries.** Every write requires the page's current `state_token`; on conflict the response carries the new tree so you reconcile in one call instead of re-viewing every time.
- **Tree deltas by default.** First view is full; subsequent views are op-stream against the last-emitted tree. Token cost stays flat as a page mutates.
- **Stable refs.** A button that survives a re-render keeps the same `Ref(N)` across snapshots, so a planned multi-step interaction doesn't fall apart on the second tick.
- **Idempotent actions.** `vs_act` keys on `(page, before_token, args_hash)` for 30s — repeat-on-flake is free, no doubled clicks.
- **Composite reads.** `vs_open --view`, `vs_view --layout=N,M`, `vs_view --read=N`, `vs_act --view`, `vs_wait --view` collapse the canonical two-call sequences into one wire frame.
- **Audit by construction.** Every primitive writes one row to `actions` before returning. Replay, debugging, compliance — all free.
- **Persistent, encrypted auth.** Cookies + storage saved as AES-256-GCM blobs keyed by an OS-keyring secret. `vs auth save` once; survive restarts.
- **Inspector capture.** `vs inspect console|network|request|eval|storage|scripts|script|dom|performance` — the page's own `console.error` flow surfaces as a wire warning before your next view.
- **One self-contained binary.** `vs serve` is the daemon. `vs <primitive>` is the client. `vs mcp` is the MCP server for Claude Desktop / Cursor / Codex. `vs skill install` writes the skill into every detected agent.

## Install

Homebrew (macOS, Linux):

```sh
brew tap frane/tap
brew install vibesurfer
```

curl (any platform with a Rust toolchain or a release binary):

```sh
curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh | sh
```

From source:

```sh
cargo install --git https://github.com/frane/vibesurfer.git vs-cli
```

System deps: nothing on macOS (uses the system WebKit). On Linux: `libwebkitgtk-6.0-dev`, `libgtk-4-dev`, `libsoup-3.0-dev`. On Windows: the WebView2 Runtime (preinstalled on Windows 11; bundled with modern Edge).

## Getting started

```sh
vs skill install
```

That writes a `SKILL.md` into every detected agent's skills directory: Claude, Codex, Cursor, OpenClaw, Smithery, the canonical `~/.agents/`. The skill teaches the agent how to drive `vs`.

You still need to tell the agent to use it. Even with the skill installed, agents fall back to their own browser tools out of habit, so something like *"use vs for browser work"* in your system prompt or your first message is what keeps them on it.

Once the agent is on `vs`, the shape that justifies the protocol shows up on any multi-step page interaction. Sign in to a SaaS, capture a screenshot, scrape a table behind the auth, leave the cookies behind for next time:

```sh
vs session-open
vs open https://app.example.com/login
vs view <page>                                 # see the a11y tree, get a state_token
vs act <page> <ref> fill alice@example.com --token=<t>
vs act <page> <ref> fill hunter2 --token=<t>   # token threads forward
vs act <page> <ref> click --token=<t>          # submit; token rejects on stale
vs wait <page> stable
vs auth save <page> work-app                   # dump cookies + storage, encrypt at rest
vs capture <page>                              # PNG to ~/.vibesurfer/captures/
vs extract <page> table --token=<t>            # table rows as records
```

The result of `vs capture` against `github.com/trending` looks like this — driven by the same `WkBackend` the M6 cell tests exercise, no fixture, no stub:

![vs capture github.com/trending](docs/img/demo-trending.png)

## Skill and MCP

`vs skill install` writes the SKILL.md to every detected agent. `vs mcp` exposes the same primitives over MCP for agents that don't have shell access — Claude Desktop, Cursor, Smithery — by speaking JSON-RPC 2.0 on stdio. Each of the 19 primitives is one MCP tool whose name matches the wire primitive (`vs_open`, `vs_view`, etc.); dispatch goes through the same `vs_cli::commands::run` path as the CLI binary, so there is no parallel engine logic, no shim, no drift.

Wire it into Claude Desktop's `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "vibesurfer": { "command": "vs", "args": ["mcp"] }
  }
}
```

Cursor, Codex, OpenClaw, Smithery — same shape, same command.

## Verification

Mac and Linux columns of `docs/REALITY_CHECK.md` are 47-of-47 cells `yes` against real engines, end-to-end, through the public CLI. Windows column is `pending-manual-verification` until the maintainer signs off on a green run from the `windows-latest` CI leg.

```sh
cargo test --test m6 -- --test-threads=1            # macOS, ~38s
docker run --rm --privileged \
  -v "$PWD":/work vs-test-linux                      # Linux, ~28s, see Dockerfile.linux-test
```

Sequential is required on every platform because every test pumps a real GUI engine on its main thread; parallel runs saturate the queue. That's documented in `docs/DEVELOPMENT.md`.

## Docs

- [Concepts](docs/ARCHITECTURE.md) — daemon, CLI, store, engine boundary
- [Protocol](docs/PROTOCOL.md) — wire format spec (single source of truth)
- [Primitives](docs/PRIMITIVES.md) — the 19 primitives, one section each
- [Codes](docs/codes.md) — role codes, viewport presets, error/warning codes
- [Reality check](docs/REALITY_CHECK.md) — current cell-by-cell verification status
- [Development](docs/DEVELOPMENT.md) — per-platform test loop
- [Decisions](docs/decisions/) — architectural decision records

## Contributing

Issues and PRs welcome. The thing I'd actually like feedback on is the inspector-capture pipeline on Windows: the JS shim that maps `webkit.messageHandlers.<name>.postMessage` onto WebView2's `chrome.webview.postMessage` works on paper, but the M6 capability-gate test
(`cell_engine_unsupported_when_install_disabled`) is the only thing locking the design and it hasn't been exercised against a real WebView2 yet. If you have a Windows box and ten minutes, the `vs-test-linux` Docker pattern translates one-for-one and the failure mode is concrete.

## License

[MIT](LICENSE).
