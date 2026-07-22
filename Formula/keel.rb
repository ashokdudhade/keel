# typed: false
# frozen_string_literal: true

# Homebrew formula for Keel. Hashes are filled by the release workflow after
# GitHub Release assets are published — do not commit placeholder checksums
# that claim to verify real archives.
class Keel < Formula
  desc "Deterministic local-first code intelligence for AI coding agents"
  homepage "https://github.com/ashokdudhade/keel"
  version "1.1.2"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "3efc77c564a842b552f05c3aaed3c0c2dcdea498781cc6abed6f3ad3d99535ce"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "aed91c59734b650fb6d82a3ef82fd23cf3ae00d6c69df6a98294eeca09e61c78"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "17e2e2c729a4ab7fad3b58adba44a87821f3896162ccc6f77c2a93088f4db8ac"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0c8474b07bc909f2406614e623755ad3d033c2a98dc21e19e7b7bf4d04717abb"
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
