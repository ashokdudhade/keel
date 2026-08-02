# typed: false
# frozen_string_literal: true

# Homebrew formula for Keel. Hashes are filled by the release workflow after
# GitHub Release assets are published — do not commit placeholder checksums
# that claim to verify real archives.
class Keel < Formula
  desc "Deterministic local-first code intelligence for AI coding agents"
  homepage "https://github.com/ashokdudhade/keel"
  version "1.3.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "eb64da9cba9b39b0513f80fb0ff4ff9a1f9ecebaf017c463701d9a50e75c1ef7"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "49e4e7c5467fcdc38d48cdc2affa611afa779d19f2c45bf0933dbbf0ba2176a6"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "7f7ece5f95c919f0d9752db26ac7b563ccdd1be05363d66cc5a5bb39f7ad0157"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "979de58338fd5fc0d5dcd93fa3a4ebf7dd0827b7596bb981175c8fe1e48ab022"
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
