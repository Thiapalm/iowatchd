# Homebrew formula for a personal tap (github.com/Thiapalm/homebrew-tap,
# file Formula/iowatchd.rb). Linux only; installs the prebuilt static binary.
# This installs the tools; run it as a service with the init files in
# share/iowatchd (systemd users: copy iowatchd.service to /etc/systemd/system).
class Iowatchd < Formula
  desc "Lightweight disk IO monitor that logs which processes cause high IO"
  homepage "https://github.com/Thiapalm/iowatchd"
  version "0.4.2"
  license "MIT"

  on_linux do
    on_intel do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.4.2/iowatchd-v0.4.2-linux-x86_64.tar.gz"
      sha256 "17b481ca72a500c3f813ebe5f7fca6c20b71ea6390550ef014efa71e229b0322"
    end
    on_arm do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.4.2/iowatchd-v0.4.2-linux-aarch64.tar.gz"
      sha256 "5b972972248c36e782d680fa77169fe4b485fc6fbd20b99f56bffb73eb8ae025"
    end
  end

  def install
    bin.install "iowatchd", "iowatchd-log", "iowatchd-notify"
    man8.install "man/iowatchd.8"
    man5.install "man/iowatchd.conf.5"
    man1.install "man/iowatchd-log.1"
    bash_completion.install "completions/iowatchd.bash" => "iowatchd"
    zsh_completion.install "completions/_iowatchd"
    fish_completion.install "completions/iowatchd.fish"
    (share/"iowatchd").install "init", "config.conf"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/iowatchd --version")
  end
end
