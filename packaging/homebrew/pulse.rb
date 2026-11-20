# A Homebrew formula for the Pulse tray app. Meant for a tap:
#   brew tap shreeyuvan2020/pulse
#   brew install pulse
# The sha256 must be filled in from the first release; until then the formula
# is a template (brew refuses to install with a missing checksum).
class Pulse < Formula
  desc "Passive time tracking for building with AI coding agents"
  homepage "https://github.com/shreeyuvan2020/pulse"
  url "https://github.com/shreeyuvan2020/pulse/releases/download/v0.1.0/pulse-macos-universal.tar.gz"
  version "0.1.0"
  sha256 "REPLACE_WITH_THE_RELEASE_SHA256"
  license "AGPL-3.0-only"

  def install
    prefix.install "Pulse.app"
    bin.install_symlink prefix/"Pulse.app/Contents/MacOS/pulse"
  end

  def caveats
    <<~EOS
      Pulse runs in the menu bar. Open it once:

        open #{prefix}/Pulse.app

      Move it to /Applications if you want it to start every day.
    EOS
  end

  test do
    system "#{bin}/pulse", "--version"
  end
end
