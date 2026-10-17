#!/usr/bin/env bash
# Compila, instala e habilita o iowatchd como servico de sistema.
# Detecta o init (systemd, OpenRC ou runit) automaticamente.
# Remove tambem a instalacao antiga chamada ssd-io-monitor, se existir.
# Uso: ./install.sh   (pede sudo apenas para as etapas de instalacao)
set -euo pipefail
cd "$(dirname "$0")"

# O cargo pode nao estar no PATH deste shell (ex.: rustup instalou em ~/.cargo/bin).
export PATH="$HOME/.cargo/bin:$PATH"
command -v cargo >/dev/null || { echo "cargo nao encontrado; instale o Rust (rustup)." >&2; exit 1; }

# Identifica o init em execucao (nao apenas o instalado).
detect_init() {
  if [ -d /run/systemd/system ]; then echo systemd
  elif command -v rc-service >/dev/null 2>&1 && [ -d /run/openrc ]; then echo openrc
  elif command -v sv >/dev/null 2>&1 && [ -d /etc/sv -o -d /etc/runit/sv ]; then echo runit
  else echo unknown
  fi
}
INIT=$(detect_init)
echo "Init detectado: $INIT"

cargo build --release

# Migracao do nome antigo (ssd-io-monitor). Preserva o log antigo.
if [ "$INIT" = systemd ] && [ -f /etc/systemd/system/ssd-io-monitor.service ]; then
  sudo systemctl disable --now ssd-io-monitor.service || true
  sudo rm -f /etc/systemd/system/ssd-io-monitor.service
fi
sudo rm -f /usr/local/bin/ssd-io-monitor /usr/local/bin/ssd-io-log /etc/ssd-io-monitor.conf

sudo install -m 755 target/release/iowatchd /usr/local/bin/iowatchd
sudo install -m 755 iowatchd-log /usr/local/bin/iowatchd-log
sudo install -D -m 644 man/iowatchd.8 /usr/local/share/man/man8/iowatchd.8
sudo install -D -m 644 man/iowatchd-log.1 /usr/local/share/man/man1/iowatchd-log.1
# Nao sobrescreve uma configuracao ja editada. device=auto detecta o disco da raiz.
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
    # Diretorio de servicos ativos varia conforme a distro.
    for d in /var/service /etc/service /run/runit/service; do
      [ -d "$d" ] && { sudo ln -sfn "$SVDIR/iowatchd" "$d/iowatchd"; break; }
    done
    ;;
  *)
    echo "Init nao suportado. Binario e configuracao instalados; configure a" >&2
    echo "inicializacao manualmente: /usr/local/bin/iowatchd -c /etc/iowatchd.conf" >&2
    ;;
esac

echo "Instalado. Ajuda: iowatchd --help | man iowatchd | iowatchd-log (-f acompanha, -s status)"
