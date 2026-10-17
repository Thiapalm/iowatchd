#!/usr/bin/env bash
# Builds, installs and enables iowatchd as a system service.
# Detects the init system (systemd, OpenRC or runit) automatically.
# Also removes the old installation named ssd-io-monitor, if present.
# Usage: ./install.sh   (asks for sudo only for the install steps)
set -euo pipefail
cd "$(dirname "$0")"

# cargo may not be in this shell's PATH (e.g. rustup installs to ~/.cargo/bin).
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo not found; install Rust (rustup)." >&2; exit 1; }

# Identify the init system that is running (not just the installed one).
detect_init() {
  if [ -d /run/systemd/system ]; then echo systemd
  elif command -v rc-service >/dev/null 2>&1 && [ -d /run/openrc ]; then echo openrc
  elif command -v sv >/dev/null 2>&1 && [ -d /etc/sv -o -d /etc/runit/sv ]; then echo runit
  else echo unknown
  fi
}
INIT=$(detect_init)
echo "Detected init: $INIT"

cargo build --release

# Migration from the old name (ssd-io-monitor). The old log is kept.
if [ "$INIT" = systemd ] && [ -f /etc/systemd/system/ssd-io-monitor.service ]; then
  sudo systemctl disable --now ssd-io-monitor.service || true
  sudo rm -f /etc/systemd/system/ssd-io-monitor.service
fi
sudo rm -f /usr/local/bin/ssd-io-monitor /usr/local/bin/ssd-io-log /etc/ssd-io-monitor.conf

sudo install -m 755 target/release/iowatchd /usr/local/bin/iowatchd
sudo install -m 755 iowatchd-log /usr/local/bin/iowatchd-log
sudo install -D -m 644 man/iowatchd.8 /usr/local/share/man/man8/iowatchd.8
sudo install -D -m 644 man/iowatchd-log.1 /usr/local/share/man/man1/iowatchd-log.1
# Does not overwrite an already edited configuration. device=auto detects the root disk.
[ -f /etc/iowatchd.conf ] || sudo install -m 644 config.conf /etc/iowatchd.conf

case "$INIT" in
  systemd)
    sudo install -m 644 init/systemd/iowatchd.service /etc/systemd/system/iowatchd.service
    sudo systemctl daemon-reload
    sudo systemctl enable iowatchd.service
    sudo systemctl restart iowatchd.service
    ;;
  openrc)
    sudo install -m 755 init/openrc/iowatchd /etc/init.d/iowatchd
    sudo rc-update add iowatchd default
    sudo rc-service iowatchd restart
    ;;
  runit)
    SVDIR=/etc/sv; [ -d /etc/runit/sv ] && SVDIR=/etc/runit/sv
    sudo install -d "$SVDIR/iowatchd"
    sudo install -m 755 init/runit/run "$SVDIR/iowatchd/run"
    # The active-services directory varies by distro.
    for d in /var/service /etc/service /run/runit/service; do
      [ -d "$d" ] && { sudo ln -sfn "$SVDIR/iowatchd" "$d/iowatchd"; break; }
    done
    ;;
  *)
    echo "Unsupported init. Binary and configuration were installed; set up" >&2
    echo "autostart manually: /usr/local/bin/iowatchd -c /etc/iowatchd.conf" >&2
    ;;
esac

echo "Installed. Help: iowatchd --help | man iowatchd | iowatchd-log (-f follow, -s status)"
