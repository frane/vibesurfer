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

# 1. Tool checks.
for tool in claude asciinema agg; do
    if ! command -v "$tool" >/dev/null; then
        echo "$tool not on PATH" >&2
        case "$tool" in
            claude) echo "  install Claude Code first." >&2 ;;
            asciinema|agg) echo "  brew install $tool" >&2 ;;
        esac
        exit 1
    fi
done

# 2. Hard TTY guard. asciinema's silent fallback is a footgun; we
# refuse rather than ship a stunted recording.
if [[ ! -t 0 || ! -t 1 ]]; then
    echo "stdin/stdout is not a TTY. Run this from a real terminal," >&2
    echo "not from inside another agent's shell." >&2
    exit 1
fi

# 3. Build artifact check.
if [[ ! -x target/release/vs ]]; then
    echo "build target/release/vs first: cargo build --release" >&2
    exit 1
fi

# 4. Tempdir for the demo workspace + cast.
DEMO_DIR=$(mktemp -d -t vs-demo-claude.XXXXXX)
CAST="$DEMO_DIR/demo.cast"

# Daemon lifecycle is owned by this script, not auto-spawn:
# auto-spawn used to misbehave when Claude's first `vs` call hit a
# fresh tempdir and the daemon hadn't bound yet. Pre-booting the
# daemon and cleaning it up on exit keeps the recording predictable.
DAEMON_PID=
cleanup() {
    if [[ -n "${DAEMON_PID}" ]]; then
        kill "$DAEMON_PID" 2>/dev/null || true
        wait 2>/dev/null || true
    fi
    rm -rf "$DEMO_DIR"
}
trap cleanup EXIT

mkdir -p "$DEMO_DIR/bin" "$DEMO_DIR/.vibesurfer"

# Shim baked in with --home + --no-spawn so the recording shows
# clean `vs <verb>` calls. --no-spawn prevents Claude's first call
# from racing the auto-spawn path.
cat > "$DEMO_DIR/bin/vs" <<SHIM
#!/usr/bin/env bash
exec "$PWD/target/release/vs" --home "$DEMO_DIR/.vibesurfer" --no-spawn "\$@"
SHIM
chmod +x "$DEMO_DIR/bin/vs"

# 5. Boot the daemon now so Claude's first `vs` call connects
# immediately to a daemon that's already bound to the demo socket.
"$PWD/target/release/vs" --home "$DEMO_DIR/.vibesurfer" serve \
    > "$DEMO_DIR/.vibesurfer/daemon.log" 2>&1 &
DAEMON_PID=$!

for _ in $(seq 1 20); do
    [[ -S "$DEMO_DIR/.vibesurfer/daemon.sock" ]] && break
    sleep 0.25
done
if [[ ! -S "$DEMO_DIR/.vibesurfer/daemon.sock" ]]; then
    echo "daemon failed to start; see $DEMO_DIR/.vibesurfer/daemon.log" >&2
    cat "$DEMO_DIR/.vibesurfer/daemon.log" >&2
    exit 1
fi

# 6. Drop the vibesurfer skill into the demo's local Claude config so
# the model finds the SKILL.md without touching the user's real
# ~/.claude.
mkdir -p "$DEMO_DIR/.claude/skills"
cp -R skills/vibesurfer "$DEMO_DIR/.claude/skills/"

# 7. Print the suggested prompt for copy-paste once recording starts.
SUGGESTED_PROMPT="Use vibesurfer to open https://news.ycombinator.com and tell me the top three stories: title, points, comments. Concise."

cat <<BANNER

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
BANNER
read -r -p "Press Enter to start, Ctrl-C to abort: " _

# 8. Record. claude is locked to Bash; MCP vibesurfer + built-in
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

# 9. Convert to gif.
agg --theme monokai --font-size 14 "$CAST" docs/demo-claude.gif

echo
echo "wrote docs/demo-claude.gif"
echo "raw cast at $CAST (will be cleaned on exit)"
