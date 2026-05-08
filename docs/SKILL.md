# Skill

The skill that gets installed into every detected agent by `vs skill install` lives at:

[`skills/vibesurfer/SKILL.md`](../skills/vibesurfer/SKILL.md)

That file is the single source of truth — it's what the agent reads, and it's the file `vs skill install` writes (verbatim) into each agent's conventional skill location:

| Agent | Path |
|---|---|
| Claude Code | `~/.claude/skills/vibesurfer/SKILL.md` |
| Codex CLI | `~/.codex/skills/vibesurfer/SKILL.md` |
| Cursor | `<workspace>/.cursor/skills/vibesurfer/SKILL.md` |
| Gemini | `~/.gemini/extensions/vibesurfer/GEMINI.md` (renamed) |
| OpenClaw | `~/.openclaw/workspace/skills/vibesurfer/SKILL.md` |
| Canonical | `~/.agents/skills/vibesurfer/SKILL.md` |

The same `vs skill install` call also writes an `mcpServers.vibesurfer = {command: "vs", args: ["mcp"]}` entry into each agent's MCP config (`.claude.json`, `~/.codex/config.toml`, `<workspace>/.cursor/mcp.json`, `~/.gemini/settings.json`, `Library/Application Support/Claude/claude_desktop_config.json` on macOS) so the agent can talk to vibesurfer over MCP as well as via the SKILL.md.
