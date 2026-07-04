# Homebrew formula for vibesurfer.
#
# Drop this into a tap (e.g. frane/homebrew-tap/Formula/vibesurfer.rb) and
# users can:
#
#     brew tap frane/tap
#     brew install vibesurfer
#
# When a release is cut, update `version`, `url`, `sha256`. The formula
# builds from source via cargo because a single binary can be built
# straight out of the workspace; if/when prebuilt release tarballs are
# published, switch to the prebuilt-bottle pattern.

class Vibesurfer < Formula
  desc "A browser for LLMs, not humans — agent-native headless WebKit + line-oriented protocol"
  homepage "https://github.com/frane/vibesurfer"
  url "https://github.com/frane/vibesurfer/archive/refs/tags/v0.1.21.tar.gz"
  # Update on each release: `shasum -a 256 <tarball>`.
  sha256 "0d9792351b4c2de9dc54ac3d2f2143cb9b363156b92080a466848cf0148eb45e"
  license "Apache-2.0"
  head "https://github.com/frane/vibesurfer.git", branch: "main"

  depends_on "rust" => :build

  on_linux do
    depends_on "gtk4"
    depends_on "webkitgtk@6"
    depends_on "libsoup@3"
    depends_on "pkg-config" => :build
  end

  def install
    system "cargo", "install", *std_cargo_args(path: "crates/vs-cli")
  end

  def caveats
    <<~EOS
      vibesurfer ships one binary, `vs`. To bootstrap the agent skill:

          vs skill install

      To wire `vs` into Claude Desktop / Cursor / Codex via MCP, add to
      the host's mcpServers config:

          { "command": "vs", "args": ["mcp"] }
    EOS
  end

  test do
    assert_match "vs ", shell_output("#{bin}/vs --version")
    assert_match "session-open", shell_output("#{bin}/vs --help")
  end
end
