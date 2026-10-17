#!/usr/bin/env bash
# Builds, installs and enables iowatchd as a system service.
# Detects the init system (systemd, OpenRC or runit) automatically.
# Also removes the old installation named ssd-io-monitor, if present.
#
# Usage:
#   ./install.sh             build, install and start the service
#   ./install.sh uninstall   stop the service and remove everything
#                            (the configuration and log are kept)
set -euo pipefail
cd "$(dirname "$0")"

# cargo may not be in this shell's PATH (e.g. rustup installs to ~/.cargo/bin).
export PATH="$HOME/.cargo/bin:$PATH"

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

runit_dirs() {
  SVDIR=/etc/sv; [ -d /etc/runit/sv ] && SVDIR=/etc/runit/sv
}

uninstall() {
  case "$INIT" in
    systemd) sudo systemctl disable --now iowatchd.service 2>/dev/null || true ;;
    openrc) sudo rc-service iowatchd stop 2>/dev/null || true
            sudo rc-update del iowatchd default 2>/dev/null || true
            sudo rm -f /etc/init.d/iowatchd ;;
    runit) runit_dirs
           sudo rm -f /var/service/iowatchd /etc/service/iowatchd /run/runit/service/iowatchd
           sudo rm -rf "$SVDIR/iowatchd" ;;
  esac
  sudo make uninstall INSTALL_INIT=systemd
  [ "$INIT" = systemd ] && sudo systemctl daemon-reload
  echo "Removed. /etc/iowatchd.conf and /var/log/iowatchd.log were kept."
}

if [ "${1:-}" = uninstall ]; then uninstall; exit 0; fi
[ $# -eq 0 ] || { echo "usage: $0 [uninstall]" >&2; exit 2; }

command -v cargo >/dev/null || { echo "cargo not found; install Rust (rustup)." >&2; exit 1; }
make build

# Migration from the old name (ssd-io-monitor). The old log is kept.
if [ "$INIT" = systemd ] && [ -f /etc/systemd/system/ssd-io-monitor.service ]; then
  sudo systemctl disable --now ssd-io-monitor.service || true
  sudo rm -f /etc/systemd/system/ssd-io-monitor.service
fi
sudo rm -f /usr/local/bin/ssd-io-monitor /usr/local/bin/ssd-io-log /etc/ssd-io-monitor.conf

# Binary, man pages, completions, logrotate and (if missing) the configuration.
# The systemd unit is installed by make only on systemd machines.
if [ "$INIT" = systemd ]; then
  sudo make install INSTALL_INIT=systemd
else
  sudo make install INSTALL_INIT=none
fi

case "$INIT" in
  systemd)
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
    runit_dirs
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
