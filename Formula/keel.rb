# typed: false
# frozen_string_literal: true

# Homebrew formula for Keel. Hashes are filled by the release workflow after
# GitHub Release assets are published — do not commit placeholder checksums
# that claim to verify real archives.
class Keel < Formula
  desc "Deterministic local-first code intelligence for AI coding agents"
  homepage "https://github.com/ashokdudhade/keel"
  version "1.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_RELEASE_SHA256_AARCH64_APPLE_DARWIN"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "REPLACE_WITH_RELEASE_SHA256_X86_64_APPLE_DARWIN"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_RELEASE_SHA256_AARCH64_UNKNOWN_LINUX_GNU"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_RELEASE_SHA256_X86_64_UNKNOWN_LINUX_GNU"
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
