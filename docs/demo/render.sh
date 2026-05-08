#!/usr/bin/env bash
# Render docs/demo.gif from docs/demo/demo.tape.
#
# Boots a real daemon, opens a real session+page against
# https://example.com, then materializes the tape with the actual
# session and page ids substituted in. This avoids putting `$(...)`
# shell substitutions in the recorded gif (vhs's tape language
# doesn't play well with bare `$` characters).
#
# Usage:
#   docs/demo/render.sh

set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

if [[ ! -x target/release/vs ]]; then
    echo "build target/release/vs first: cargo build --release" >&2
    exit 1
fi

if ! command -v vhs >/dev/null; then
    echo "vhs not on PATH; brew install vhs" >&2
    exit 1
fi

VS_HOME=/tmp/vs-demo
BIN_DIR="$VS_HOME/bin"
RENDERED_TAPE="$VS_HOME/demo.tape"
export VS_HOME

rm -rf "$VS_HOME"
mkdir -p "$VS_HOME" "$BIN_DIR"

# Shim that bakes in --home + --no-spawn so the gif shows clean
# `vs <verb>` invocations.
cat > "$BIN_DIR/vs" <<EOF
#!/usr/bin/env bash
exec "$PWD/target/release/vs" --home "$VS_HOME" --no-spawn "\$@"
EOF
chmod +x "$BIN_DIR/vs"

# Boot the daemon.
"$PWD/target/release/vs" --home "$VS_HOME" serve > "$VS_HOME/daemon.log" 2>&1 &
DAEMON_PID=$!
trap 'kill $DAEMON_PID 2>/dev/null || true; wait 2>/dev/null || true' EXIT

for _ in $(seq 1 20); do
    [[ -S "$VS_HOME/daemon.sock" ]] && break
    sleep 0.25
done
if [[ ! -S "$VS_HOME/daemon.sock" ]]; then
    echo "daemon failed to start; see $VS_HOME/daemon.log" >&2
    cat "$VS_HOME/daemon.log" >&2
    exit 1
fi

# Open a real session + page so the tape can substitute their ids.
# session-open output's last line is the session id.
SESSION_OUT=$("$BIN_DIR/vs" session-open)
SESSION=$(echo "$SESSION_OUT" | tail -1)
PAGE_OUT=$("$BIN_DIR/vs" open https://news.ycombinator.com)
PAGE=$(echo "$PAGE_OUT" | tail -1)
"$BIN_DIR/vs" wait "$PAGE" stable 5000 >/dev/null 2>&1 || true

if [[ -z "$SESSION" || -z "$PAGE" ]]; then
    echo "failed to open session+page; daemon log:" >&2
    cat "$VS_HOME/daemon.log" >&2
    exit 1
fi

echo "session=$SESSION page=$PAGE"

# Materialize the tape.
sed \
    -e "s|__BIN_DIR__|$BIN_DIR|g" \
    -e "s|__SESSION__|$SESSION|g" \
    -e "s|__PAGE__|$PAGE|g" \
    docs/demo/demo.tape > "$RENDERED_TAPE"

vhs "$RENDERED_TAPE"

echo
echo "wrote docs/demo.gif"
