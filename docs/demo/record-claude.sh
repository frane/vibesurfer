#!/usr/bin/env bash
# Record a real interactive Claude Code session driving vibesurfer.
#
# Output: `docs/demo-claude.gif`. Claude is locked to Bash (the
# vibesurfer skill's shell-driven path), with MCP and built-in file
# tools off, so the agent is forced to use the real `vs` CLI.
#
# Run this from a real terminal. asciinema silently falls back to
# headless mode without a controlling TTY and produces a stunted
# recording — that's the most common "the gif looks broken" failure.
# Don't run from inside another Claude / Codex session: auth state
# collides too.
#
# Reproducibility caveat: each render produces a different recording.
# Model output varies, tool-call ordering varies. The cached gif at
# `docs/demo-claude.gif` is committed so casual cloners and CI don't
# re-render.

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

# 1. Check `claude` on PATH.
if ! command -v claude >/dev/null; then
    echo "claude not on PATH; install Claude Code first." >&2
    exit 1
fi
if ! command -v asciinema >/dev/null; then
    echo "asciinema not on PATH; brew install asciinema." >&2
    exit 1
fi
if ! command -v agg >/dev/null; then
    echo "agg not on PATH; brew install agg." >&2
    exit 1
fi

# 2. Hard TTY guard. asciinema's silent fallback is a footgun; we
# refuse rather than ship a stunted recording.
if [[ ! -t 0 || ! -t 1 ]]; then
    echo "stdin/stdout is not a TTY. Run this from a real terminal," >&2
    echo "not from inside another agent's shell." >&2
    exit 1
fi

# 3. Tempdir for the demo workspace + cast.
DEMO_DIR=$(mktemp -d -t vs-demo-claude.XXXXXX)
CAST="$DEMO_DIR/demo.cast"
trap 'rm -rf "$DEMO_DIR"' EXIT

# 4. Off-camera setup. The cached release binary needs to be on PATH;
# we drop a shim into the demo dir so the recording shows clean
# `vs <verb>` calls and the demo home is isolated.
if [[ ! -x target/release/vs ]]; then
    echo "build target/release/vs first: cargo build --release" >&2
    exit 1
fi
mkdir -p "$DEMO_DIR/bin" "$DEMO_DIR/.vibesurfer"
cat > "$DEMO_DIR/bin/vs" <<EOF
#!/usr/bin/env bash
exec "$PWD/target/release/vs" --home "$DEMO_DIR/.vibesurfer" "\$@"
EOF
chmod +x "$DEMO_DIR/bin/vs"

# Drop the vibesurfer skill into the demo dir so Claude finds it
# without polluting the user's real ~/.claude. SKILL.md teaches the
# agent how to drive `vs` via Bash.
mkdir -p "$DEMO_DIR/.claude/skills"
cp -R skills/vibesurfer "$DEMO_DIR/.claude/skills/"

# 5. Print the suggested prompt. The user copies this once recording
# starts.
SUGGESTED_PROMPT="Use vibesurfer to open https://news.ycombinator.com and tell me the top three stories: title, points, comments. Concise."

cat <<EOF

==========================================================================
  Ready to record a real Claude Code session driving vibesurfer.

  When you press Enter:
    - asciinema starts recording
    - claude launches inside $DEMO_DIR
    - paste this prompt:

      $SUGGESTED_PROMPT

    - watch the agent reason and call \`vs\` over Bash
    - type /exit when done; recording stops automatically

  The cast goes to $CAST, then converts to docs/demo-claude.gif.
==========================================================================
EOF
read -r -p "Press Enter to start, Ctrl-C to abort: " _

# 7. Record. claude is locked to Bash; MCP vibesurfer + built-in
# file tools are denied, so the agent is forced to use the `vs`
# binary on PATH. --idle-time-limit 2 trims dead air; --cols/--rows
# pin the recording shape so the gif renders predictably.
asciinema rec \
    --idle-time-limit 2 \
    --cols 110 \
    --rows 36 \
    --overwrite \
    --command "cd '$DEMO_DIR' && PATH='$DEMO_DIR/bin':\$PATH claude --allowed-tools Bash --disallowed-tools 'Read,Edit,Write,mcp__vibesurfer__*'" \
    "$CAST"

# 8. Convert to gif.
agg --theme monokai --font-size 14 "$CAST" docs/demo-claude.gif

echo
echo "wrote docs/demo-claude.gif"
echo "raw cast at $CAST (will be cleaned on exit)"
