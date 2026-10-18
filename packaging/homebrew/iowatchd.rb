# Homebrew formula for a personal tap (github.com/Thiapalm/homebrew-tap,
# file Formula/iowatchd.rb). Linux only; installs the prebuilt static binary.
# This installs the tools; run it as a service with the init files in
# share/iowatchd (systemd users: copy iowatchd.service to /etc/systemd/system).
class Iowatchd < Formula
  desc "Lightweight disk IO monitor that logs which processes cause high IO"
  homepage "https://github.com/Thiapalm/iowatchd"
  license "MIT"

  on_linux do
    on_intel do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.5.2/iowatchd-v0.5.2-linux-x86_64.tar.gz"
      sha256 "f5f862df67af2273b0e905578caa43aaf7df0f6c61921126c585cf52120aba5a"
    end
    on_arm do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.5.2/iowatchd-v0.5.2-linux-aarch64.tar.gz"
      sha256 "de9662bf983430c29d12f482b69b2d8c196c385f8b799c202cac032f513dd971"
    end
  end

  def install
    bin.install "iowatchd", "iowatchd-log", "iowatchd-notify"
    man8.install "man/iowatchd.8"
    man5.install "man/iowatchd.conf.5"
    man1.install "man/iowatchd-log.1", "man/iowatchd-notify.1"
    bash_completion.install "completions/iowatchd.bash" => "iowatchd"
    zsh_completion.install "completions/_iowatchd"
    fish_completion.install "completions/iowatchd.fish"
    pkgshare.install "init", "config.conf"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/iowatchd --version")
  end
end
