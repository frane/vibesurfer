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
  desc "A browser for LLMs, not humans."
  homepage "https://github.com/frane/vibesurfer"
  url "https://github.com/frane/vibesurfer/archive/refs/tags/v0.2.19.tar.gz"
  # Update on each release: `shasum -a 256 <tarball>`.
  sha256 "5b56c59b078bab5ed7b410b6f03362782e404f5b93e78908d8c6acd6db58579a"
  license "Apache-2.0"
  head "https://github.com/frane/vibesurfer.git", branch: "main"

  depends_on "rust" => :build
  # openh264 (recording encoder) compiles from vendored source via nasm.
  depends_on "nasm" => :build
  # WebM recordings: libvpx, found with pkg-config; bindgen needs libclang.
  depends_on "llvm" => :build
  depends_on "pkgconf" => :build
  depends_on "libvpx"

  on_linux do
    depends_on "gtk4"
    depends_on "webkitgtk@6"
    depends_on "libsoup@3"
  end

  def install
    ENV["LIBCLANG_PATH"] = Formula["llvm"].opt_lib.to_s
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
