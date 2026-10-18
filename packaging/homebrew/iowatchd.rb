# Homebrew formula for a personal tap (github.com/Thiapalm/homebrew-tap,
# file Formula/iowatchd.rb). Linux only; installs the prebuilt static binary.
# This installs the tools; run it as a service with the init files in
# share/iowatchd (systemd users: copy iowatchd.service to /etc/systemd/system).
class Iowatchd < Formula
  desc "Lightweight disk IO monitor that logs which processes cause high IO"
  homepage "https://github.com/Thiapalm/iowatchd"
  version "0.5.0"
  license "MIT"

  on_linux do
    on_intel do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.5.0/iowatchd-v0.5.0-linux-x86_64.tar.gz"
      sha256 "06fea295f7f5ae6164b751241072155bfcfb202712ad6e69ebb3b9ad44b40d32"
    end
    on_arm do
      url "https://github.com/Thiapalm/iowatchd/releases/download/v0.5.0/iowatchd-v0.5.0-linux-aarch64.tar.gz"
      sha256 "0985f3330374023a3cf87f3a3f7e0ab29be0ac87b690cc7771ca94843a450c84"
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
