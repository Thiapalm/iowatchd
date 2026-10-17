# iowatchd

Monitora o uso de IO do disco (`%util`, igual ao `iostat -x`) e, quando passa de 80%, registra os processos que mais leem/escrevem.

- Rust puro, sem dependências: ~360 KB, ~2,5 MB de RAM, CPU ~0%.
- Lê só `/proc/diskstats` a cada 2 s; a varredura de `/proc/<pid>/io` ocorre apenas durante o alerta.
- Log no journald e em `/var/log/iowatchd.log` (rotação em 5 MB).
- Serviço de boot detectado automaticamente: systemd (testado), OpenRC e runit (não testados). Em outros inits, o `install.sh` instala só o binário e indica o comando.
- Disco detectado automaticamente (`device = auto`): o disco físico que contém `/`, mesmo com partições, LVM ou LUKS (SATA, NVMe etc.).
- No systemd: `Nice=19`, IO `idle`, `CPUQuota=5%`, `MemoryMax=20M`.

## Instalação

    ./install.sh

Se existir a instalação antiga (`ssd-io-monitor`), ela é removida.

## Uso

    iowatchd --help                 opções
    iowatchd --once                 uma medição e sai
    iowatchd -d nvme0n1 -t 60       sobrescreve disco e limite
    man iowatchd                    manual completo

Opções (`-c`, `-d`, `-t`, `-i`, `-n`, `-o`, `-h`, `-V`) têm prioridade sobre o arquivo de configuração. Códigos de saída: 0 sucesso, 1 erro de execução, 2 erro de uso. `SIGHUP` recarrega a configuração (`sudo systemctl reload iowatchd`); `SIGTERM`/`SIGINT` encerram de forma limpa.

Atalho para os logs: `iowatchd-log` (últimos alertas), `iowatchd-log -f` (tempo real), `iowatchd-log -s` (status).

Comandos equivalentes:

    systemctl status iowatchd
    journalctl -u iowatchd -f
    tail -f /var/log/iowatchd.log

Exemplo de registro:

    ALERTA IO=94% em sda | 1) dd pid=29847 user=thiago R=0KB/s W=177.4MB/s cmd="dd if=..." | 2) ...
    IO normalizado: 0% em sda

## Configuração

`/etc/iowatchd.conf` (modelo em `config.conf`). Após editar: `sudo systemctl restart iowatchd`.

## Desenvolvimento

    cargo test
    cargo build --release
