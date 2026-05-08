# vibesurfer

A browser for AI agents. ~50 tokens per turn instead of ~2000.

```
$ vs open https://billing.example.com
@a1
p_1 https://billing.example.com
1 tf Email
2 tf Password
3 btn "Sign in"

$ vs act 1 fill "frane@example.com"
@a2
~1 val=frane@example.com

$ vs act 3 click
@b1
?nav
+5 lnk Invoices

$ vs act 5 click
@c1
+7 btn "Download latest"
```

That whole flow is ~150 input tokens. Same flow over Playwright is ~2000.

[![m6](https://github.com/frane/vibesurfer/actions/workflows/m6.yml/badge.svg)](https://github.com/frane/vibesurfer/actions/workflows/m6.yml) [![CI](https://github.com/frane/vibesurfer/actions/workflows/ci.yml/badge.svg)](https://github.com/frane/vibesurfer/actions/workflows/ci.yml) ![License](https://img.shields.io/badge/license-MIT-blue)

## What

Sync line protocol. State tokens for freshness. Tree deltas, not full DOM dumps. 20 primitives. Audit log in SQLite. Three real backends (WKWebView, WebKitGTK, WebView2).

## Why

CDP was designed for humans staring at DevTools. Agents are not humans. They pay per token, block per response, race on async events. vibesurfer is what the protocol looks like when the caller is the agent. ([more](docs/RATIONALE.md))

## How

```
git clone https://github.com/frane/vibesurfer
cd vibesurfer
cargo install --path crates/vs-cli

vs session-open
vs open https://example.com --view
vs skill install      # configures every detected agent (Claude, Codex, Cursor, Gemini, OpenClaw)
```

Daemon auto-spawns on first call. Linux needs WebKitGTK 6. Windows needs WebView2 runtime.

Homebrew (macOS, Linux): `brew tap frane/tap && brew install vibesurfer`.
curl: `curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh | sh`.

## Compared to

| | Wire shape | Tokens / turn |
|---|---|---|
| Playwright over CDP | Async events, target juggling | ~2000 |
| Lightpanda | CDP, less RAM | ~2000 |
| Browser Use / Stagehand | A11y tree over CDP | ~800 |
| **vibesurfer** | **Sync line protocol** | **~50** |

## Docs

- [PROTOCOL.md](docs/PROTOCOL.md) wire format
- [PRIMITIVES.md](docs/PRIMITIVES.md) the 20 calls
- [SKILL.md](skills/vibesurfer/SKILL.md) agent bootstrap, ~600 tokens
- [RATIONALE.md](docs/RATIONALE.md) why this exists
- [DEVELOPMENT.md](docs/DEVELOPMENT.md) building and testing
- [REALITY_CHECK.md](docs/REALITY_CHECK.md) per-platform verification status

MIT.
