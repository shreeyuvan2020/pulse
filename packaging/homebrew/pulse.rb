# A Homebrew formula for the Pulse tray app. Meant for a tap:
#   brew tap shreeyuvan2020/pulse
#   brew install pulse
# The sha256 is of the v0.1.0 macos tarball; bump the url, version and
# checksum together on each release.
class Pulse < Formula
  desc "Passive time tracking for building with AI coding agents"
  homepage "https://github.com/shreeyuvan2020/pulse"
  url "https://github.com/shreeyuvan2020/pulse/releases/download/v0.1.0/pulse-macos-universal.tar.gz"
  version "0.1.0"
  sha256 "1bd0465a4d28758d8a3eca1a84d067459f4c9df2fd8d5682ec22859ad1416b26"
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
