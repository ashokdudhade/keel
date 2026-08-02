# typed: false
# frozen_string_literal: true

# Homebrew formula for Keel. Hashes are filled by the release workflow after
# GitHub Release assets are published — do not commit placeholder checksums
# that claim to verify real archives.
class Keel < Formula
  desc "Deterministic local-first code intelligence for AI coding agents"
  homepage "https://github.com/ashokdudhade/keel"
  version "1.2.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "d9d86368d3bce11e2234a6f5fd10847f124f98ae39b4323d8daeea0442f1814e"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "9d2cfced13e4a650b7792bdd5638ab0ed6991dacc53192a1e5928ba15ce7f043"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "1a0d449acb04473f84821bdc3854d951a166fa1afe026dd230169d253ebbeab5"
    end
    on_intel do
      url "https://github.com/ashokdudhade/keel/releases/download/v#{version}/keel-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "dd89d8ca41f7e3ffd573eabcb0911aae7ba70707f29e257e91f52fc81dcde488"
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
