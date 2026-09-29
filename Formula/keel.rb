# typed: false
# frozen_string_literal: true

# Homebrew formula for Keel. Hashes are filled by the release workflow after
# GitHub Release assets are published — do not commit placeholder checksums
# that claim to verify real archives.
class Keel < Formula
  desc "Deterministic local-first code intelligence for AI coding agents"
  homepage "https://github.com/ashokdudhade/keel"
  version "1.4.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "8add3286fa5a27f55a5e577bfc1aecb4cfc0b8b2a239124197b72a1421d402a5"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "a561575107087114663a3569317fea6d52f9b6275d87eaa2ce9e08d9961825a1"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "cfc788c796a80ca6de7285afdfaf37180fb1ad2b605579c60920599c054cfb86"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "eea299cd1f4e87506dee40b9983ca2bd0fcf3830eb779b66b6194ea3bc02b8d0"
    end
  end

  def install
    bin.install "keel"
  end

  # Global daemon: brew services start keel → then per-project keel start.
  service do
    run [opt_bin/"keel", "daemon"]
    keep_alive true
    log_path var/"log/keel.log"
    error_log_path var/"log/keel.err.log"
  end

  test do
    assert_match "keel", shell_output("#{bin}/keel --help")
  end

  def caveats
    <<~EOS
      Keel uses a global daemon plus per-project indexes (.keel/index.db).

      Recommended:
        brew services start keel
        cd /path/to/project
        keel start
        keel definition SomeSymbol
        keel stop

      Queries auto-run a fast incremental index when needed.
      Use --no-auto-index to skip that.

      Foreground daemon (without brew):
        keel daemon
    EOS
  end
end
