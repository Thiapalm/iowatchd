# iowatchd

Monitors disk IO usage (`%util`, the same as `iostat -x`) and, when it goes above 80%, logs the processes doing the most reads/writes.

- Pure Rust, no dependencies: ~360 KB binary, ~2.5 MB of RAM, ~0% CPU.
- Reads only `/proc/diskstats` every 2 s; the `/proc/<pid>/io` scan happens only during an alert.
- Logs to journald and to `/var/log/iowatchd.log` (rotated at 5 MB).
- Boot service picked automatically: systemd (tested), OpenRC and runit (untested). On other init systems, `install.sh` installs only the binary and prints the command to run.
- Automatic disk detection (`device = auto`): the physical disk that holds `/`, even with partitions, LVM or LUKS (SATA, NVMe, etc.).
- Under systemd: `Nice=19`, idle IO class, `CPUQuota=5%`, `MemoryMax=20M`.

## Installation

From source (detects the init system, installs and starts the service):

    ./install.sh
    ./install.sh uninstall      # keeps /etc/iowatchd.conf and the log

Or with make (`PREFIX`, `DESTDIR`, `SYSTEMDUNITDIR` are honoured):

    make && sudo make install

Packaging recipes are in `packaging/`: Arch (`PKGBUILD`), RPM (`iowatchd.spec`) and Debian (`debian/`; copy it to the source root and run `dpkg-buildpackage -us -uc`). The RPM and Debian recipes are untested.

If the old installation (`ssd-io-monitor`) exists, `install.sh` removes it. Man pages (`iowatchd(8)`, `iowatchd.conf(5)`, `iowatchd-log(1)`), bash/zsh/fish completions and a logrotate file are installed too.

## Usage

    iowatchd --help                 options
    iowatchd --once                 one measurement, then exit
    iowatchd --once --json          same, as a JSON line
    iowatchd --check-config         validate and print the configuration
    iowatchd -d nvme0n1 -t 60       override disk and threshold
    man iowatchd                    full manual

Options (`-c -d -t -i -n -j -o -C -f -q -v -h -V`) take precedence over the configuration file. Exit codes: 0 success, 1 runtime error, 2 usage error. `SIGHUP` reloads the configuration (`sudo systemctl reload iowatchd`); `SIGTERM`/`SIGINT` stop it cleanly. Under systemd the unit is `Type=notify`.

Log shortcut: `iowatchd-log` (latest alerts), `iowatchd-log -f` (follow), `iowatchd-log -s` (status).

Equivalent commands:

    systemctl status iowatchd
    journalctl -u iowatchd -f
    tail -f /var/log/iowatchd.log

Example log entries:

    ALERT IO=94% on sda | 1) dd pid=29847 user=thiago R=0KB/s W=177.4MB/s cmd="dd if=..." | 2) ...
    IO back to normal: 0% on sda

## Configuration

`/etc/iowatchd.conf` (template in `config.conf`, documented in `man iowatchd.conf`). After editing: `sudo systemctl reload iowatchd`. Set `log_format = json` for JSON lines, or `log_max_mb = 0` to let logrotate manage the file.

## Development

    cargo fmt --check && cargo clippy --all-targets && cargo test

## License

MIT. See [LICENSE](LICENSE).
