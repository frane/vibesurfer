#!/usr/bin/env bash
# Mirror the canonical agent-facing skills into `plugin/` so the
# plugin-marketplace channels (Claude Code, Codex, Gemini) see the
# same content the `vs skill install` binary writes. The top-level
# `skills/` directory is the source of truth — this script is a
# one-way copy and is also run by CI to fail PRs that touched
# `skills/` without updating the plugin mirror.

set -eu

cd "$(dirname "$0")/.."

cp skills/vibesurfer/SKILL.md crates/vs-cli/SKILL.md
cp skills/vibesurfer/SKILL.md plugin/skills/vibesurfer/SKILL.md
cp skills/debug-failed-action/SKILL.md plugin/skills/debug-failed-action/SKILL.md
cp skills/debug-failed-action/skill.toml plugin/skills/debug-failed-action/skill.toml

# Mirror the canonical README into every published crate so crates.io
# renders it on each package page.
for c in vs-cli vs-daemon vs-engine-webkit vs-humanize vs-protocol vs-store; do
  cp README.md "crates/$c/README.md"
done

echo "plugin/ mirror is up to date"
