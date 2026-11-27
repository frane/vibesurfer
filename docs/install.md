# Installing vibesurfer

## No install

`npx` downloads the prebuilt binary for your platform (checksum-verified, then cached):

```
npx vibesurfer session-open
npx vibesurfer open https://example.com
```

## Package managers and scripts

| | |
|---|---|
| Homebrew (macOS, Linux) | `brew tap frane/tap && brew install vibesurfer` |
| curl (macOS, Linux) | `curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh \| sh` |
| PowerShell (Windows) | `irm https://raw.githubusercontent.com/frane/vibesurfer/main/install.ps1 \| iex` |
| Cargo | `cargo install vibesurfer` |

Linux needs WebKitGTK 6, and Xvfb is recommended (`apt install xvfb`): with it the daemon runs page windows on a display of its own, so they never show on your screen, trusted input never moves your pointer, and parallel pages keep their own pointer and hover on any monitor. It needs no `DISPLAY`, so it also runs on a server. Without Xvfb, page windows go on the host display. `VS_DISPLAY=host` picks the host display anyway; `VS_XVFB_SCREEN=WxH` sizes the private one (default `8016x5016`, room for 24 pages apart, about 150 MB). Windows needs the WebView2 runtime (already on Windows 11, available for Windows 10 from Microsoft). The prebuilt binaries carry libvpx for WebM recordings; building with Cargo needs it installed (`brew install libvpx`; `apt install libvpx-dev libclang-dev`; `vcpkg install libvpx` on Windows with `VPX_LIB_DIR` set).

## Wiring it into an agent

There are two integration paths, independent of each other:

- **Skill**: `SKILL.md` in the agent's skills directory. The agent reads it and calls the `vs` binary through its shell. For any agent that runs Bash.
- **MCP**: `vs mcp` registered as an MCP server. The agent calls the same commands as MCP tools, no shell needed.

`vs skill install` does both where supported. It detects Claude Desktop, Claude Code, Cursor, Codex CLI, Google Antigravity and OpenClaw and writes each one what it can use. Run it again after upgrading.

By hand, for the skill: copy `skills/vibesurfer/SKILL.md` to the agent's skills directory (for Claude agents, `~/.claude/skills/vibesurfer/SKILL.md`). For MCP, add this to the agent's MCP config (`claude_desktop_config.json`, `.cursor/mcp.json`, ...):

```json
{
  "mcpServers": {
    "vibesurfer": { "command": "vs", "args": ["mcp"] }
  }
}
```

Codex uses TOML with the same shape under `[mcp_servers.vibesurfer]`. The JSON is also in the repo at `plugin/.mcp.json`.

In Claude Code, the plugin marketplace installs both at once:

```
/plugin install frane/vibesurfer
```

## Building from source

Requires Rust 1.89+.

- **macOS** (15+): nothing extra; links against system WebKit.
- **Linux**: `libwebkitgtk-6.0-dev`, `libgtk-4-dev`, `libsoup-3.0-dev`, plus libvpx as above.
- **Windows**: the WebView2 SDK comes with `webview2-com` at build time; the WebView2 Runtime is needed at run time.

```
git clone https://github.com/frane/vibesurfer && cd vibesurfer
cargo install --path crates/vs-cli
```

Tests:

```
cargo test --workspace --lib --bins        # unit tests
cargo test --workspace                     # adds integration tests on the real engine
```

For Linux engine tests on another host, use the Docker container. WebKitGTK 6's sandbox needs unprivileged user namespaces, which takes `--privileged` under Docker:

```
docker build -f Dockerfile.linux-test -t vs-test-linux .
docker run --rm --privileged -v "$PWD":/work vs-test-linux
```

[DEVELOPMENT.md](DEVELOPMENT.md) has the longer walkthrough, including how the demo gif is recorded.
