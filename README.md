# iowatchd

Monitors disk IO usage (`%util`, the same as `iostat -x`) and, when it goes above 80%, logs the processes doing the most reads/writes.

- Pure Rust, no dependencies: ~360 KB binary, ~2.5 MB of RAM, ~0% CPU.
- Reads only `/proc/diskstats` every 2 s; the `/proc/<pid>/io` scan happens only during an alert.
- Logs to journald and to `/var/log/iowatchd.log` (rotated at 5 MB).
- Boot service picked automatically: systemd (tested), OpenRC and runit (untested). On other init systems, `install.sh` installs only the binary and prints the command to run.
- Automatic disk detection (`device = auto`): the physical disk that holds `/`, even with partitions, LVM or LUKS (SATA, NVMe, etc.).
- Under systemd: `Nice=19`, idle IO class, `CPUQuota=5%`, `MemoryMax=20M`.

## Installation

    ./install.sh

If the old installation (`ssd-io-monitor`) exists, it is removed.

## Usage

    iowatchd --help                 options
    iowatchd --once                 one measurement, then exit
    iowatchd -d nvme0n1 -t 60       override disk and threshold
    man iowatchd                    full manual

Options (`-c`, `-d`, `-t`, `-i`, `-n`, `-o`, `-h`, `-V`) take precedence over the configuration file. Exit codes: 0 success, 1 runtime error, 2 usage error. `SIGHUP` reloads the configuration (`sudo systemctl reload iowatchd`); `SIGTERM`/`SIGINT` stop it cleanly.

Log shortcut: `iowatchd-log` (latest alerts), `iowatchd-log -f` (follow), `iowatchd-log -s` (status).

Equivalent commands:

    systemctl status iowatchd
    journalctl -u iowatchd -f
    tail -f /var/log/iowatchd.log

Example log entries:

    ALERT IO=94% on sda | 1) dd pid=29847 user=thiago R=0KB/s W=177.4MB/s cmd="dd if=..." | 2) ...
    IO back to normal: 0% on sda

## Configuration

`/etc/iowatchd.conf` (template in `config.conf`). After editing: `sudo systemctl restart iowatchd`.

## Development

    cargo test
    cargo build --release

## License

MIT. See [LICENSE](LICENSE).
