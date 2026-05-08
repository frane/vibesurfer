#!/usr/bin/env sh
# vibesurfer (`vs`) installer.
#
# Tries each path in order:
#   1. Latest GitHub release binary for the host triple.
#   2. `cargo install --git` if a Rust toolchain is on PATH.
#   3. Print clear next steps and exit non-zero.
#
# Usage:
#   curl -sSL https://raw.githubusercontent.com/frane/vibesurfer/main/install.sh | sh
#   curl -sSL .../install.sh | sh -s -- --version v0.1.0
#   curl -sSL .../install.sh | sh -s -- --bin-dir ~/.local/bin

set -eu

REPO="frane/vibesurfer"
BIN_NAME="vs"
DEFAULT_VERSION="latest"
DEFAULT_BIN_DIR=""

VERSION="$DEFAULT_VERSION"
BIN_DIR="$DEFAULT_BIN_DIR"
while [ $# -gt 0 ]; do
    case "$1" in
        --version) VERSION="$2"; shift 2 ;;
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --help|-h)
            cat <<EOF
Usage: install.sh [--version v0.1.0] [--bin-dir ~/.local/bin]

Without --bin-dir, picks the first writable directory in PATH:
    \$HOME/.local/bin, /usr/local/bin, /opt/homebrew/bin.
EOF
            exit 0 ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

say() { printf "vibesurfer-install: %s\n" "$1"; }
fail() { printf "vibesurfer-install: %s\n" "$1" >&2; exit 1; }

# ---- pick a bin dir --------------------------------------------------------
pick_bin_dir() {
    [ -n "$BIN_DIR" ] && { echo "$BIN_DIR"; return; }
    for d in "$HOME/.local/bin" "/usr/local/bin" "/opt/homebrew/bin"; do
        if [ -d "$d" ] && [ -w "$d" ]; then
            echo "$d"
            return
        fi
        # Try to create $HOME/.local/bin if missing.
        if [ "$d" = "$HOME/.local/bin" ] && mkdir -p "$d" 2>/dev/null && [ -w "$d" ]; then
            echo "$d"
            return
        fi
    done
    fail "no writable bin dir found; pass --bin-dir <path>"
}

# ---- detect platform -------------------------------------------------------
detect_target() {
    os=$(uname -s)
    arch=$(uname -m)
    case "$os" in
        Darwin) os_t="apple-darwin" ;;
        Linux)  os_t="unknown-linux-gnu" ;;
        MINGW*|MSYS*|CYGWIN*) os_t="pc-windows-gnu" ;;
        *) fail "unsupported OS: $os" ;;
    esac
    case "$arch" in
        x86_64|amd64) arch_t="x86_64" ;;
        arm64|aarch64) arch_t="aarch64" ;;
        *) fail "unsupported arch: $arch" ;;
    esac
    echo "${arch_t}-${os_t}"
}

# ---- try downloading a release binary -------------------------------------
try_release() {
    target="$1"
    ver="$2"
    bin="$3"

    if [ "$ver" = "latest" ]; then
        url_api="https://api.github.com/repos/${REPO}/releases/latest"
        ver=$(curl -sSL "$url_api" | sed -n 's/.*"tag_name": *"\([^"]*\)".*/\1/p' | head -1)
        if [ -z "$ver" ]; then
            return 1
        fi
    fi

    # Asset name convention: vs-<version>-<target>.tar.gz
    asset="vs-${ver}-${target}.tar.gz"
    url="https://github.com/${REPO}/releases/download/${ver}/${asset}"

    say "trying release: $url"
    tmp=$(mktemp -d)
    if ! curl -sSLfo "$tmp/$asset" "$url" 2>/dev/null; then
        rm -rf "$tmp"
        return 1
    fi

    tar -xzf "$tmp/$asset" -C "$tmp"
    if [ ! -x "$tmp/$BIN_NAME" ]; then
        rm -rf "$tmp"
        return 1
    fi

    install -m 0755 "$tmp/$BIN_NAME" "$bin/$BIN_NAME"
    rm -rf "$tmp"
    say "installed $bin/$BIN_NAME from release $ver"
    return 0
}

# ---- fallback: cargo install ----------------------------------------------
try_cargo() {
    bin="$1"
    if ! command -v cargo >/dev/null 2>&1; then
        return 1
    fi
    say "no release binary available; building from source via cargo install"
    cargo install --git "https://github.com/${REPO}.git" --bin "$BIN_NAME" --root "${bin%/bin}" || return 1
    say "installed $bin/$BIN_NAME from source"
    return 0
}

# ---- main ------------------------------------------------------------------
target=$(detect_target)
bin_dir=$(pick_bin_dir)
say "host target: $target"
say "bin dir: $bin_dir"

if try_release "$target" "$VERSION" "$bin_dir"; then
    :
elif try_cargo "$bin_dir"; then
    :
else
    cat >&2 <<EOF
vibesurfer-install: could not install $BIN_NAME.
Tried: GitHub release for $target, cargo install --git.
Either:
  - install Rust (https://rustup.rs) and re-run, or
  - download the appropriate asset from
    https://github.com/${REPO}/releases
    and extract \`$BIN_NAME\` into a directory on your PATH.
EOF
    exit 1
fi

# ---- verify & next steps ---------------------------------------------------
case ":$PATH:" in
    *":$bin_dir:"*) ;;
    *) say "warning: $bin_dir is not on \$PATH; add it before running 'vs'." ;;
esac

if "$bin_dir/$BIN_NAME" --version >/dev/null 2>&1; then
    say "ok: $($bin_dir/$BIN_NAME --version)"
else
    fail "binary did not run cleanly; inspect $bin_dir/$BIN_NAME"
fi

cat <<EOF
Next:
  vs skill install     # write SKILL.md into every detected agent
  vs session-open      # start a session
  vs --help            # full command list

Wire vs into Claude Desktop / Cursor / Codex via MCP:
  Add to mcpServers config: { "command": "vs", "args": ["mcp"] }
EOF
