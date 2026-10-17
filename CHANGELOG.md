# Changelog

All notable changes to this project are documented here.
The format follows [Keep a Changelog](https://keepachangelog.com/) and the
project uses [Semantic Versioning](https://semver.org/).

## [0.3.0] - 2026-10-17

### Added
- Argument parser: grouped short flags (`-jq`), attached values (`-t80`),
  `--` to end options and "did you mean" hints for mistyped long options.
- `--list-devices` lists the monitorable disks and marks the `auto` choice.
- `$IOWATCHD_CONFIG` and `<config>.d/*.conf` drop-in overrides.
- `SIGUSR1` logs the current status (a `status` event).
- Warning when running without privileges to inspect all processes.
- `--version` shows license and project URL.
- `iowatchd-log`: `-n/--lines`, `--since`, `--json`, `--no-pager`, and it now
  follows `log_file` from the configuration (journal fallback when empty).
- Release workflow: static musl binaries (x86_64, aarch64) with checksums
  attached to GitHub releases; optional crates.io publishing.
- CI builds the Debian and RPM packages.

### Fixed
- `iowatchd --once | head` no longer panics: SIGPIPE has its default action.

## [0.2.0] - 2026-10-17

### Added
- Command-line interface: `-c -d -t -i -n -j -o -C -f -q -v -h -V`, with
  documented exit codes (0 success, 1 runtime error, 2 usage error).
- `SIGHUP` reloads the configuration; `SIGTERM`/`SIGINT` stop cleanly.
- `--json` / `log_format = json`: one JSON object per line.
- `--check-config` validates and prints the effective configuration.
- systemd `Type=notify` support (`READY=1`, `STATUS=`, `STOPPING=1`).
- `log_max_mb = 0` disables built-in rotation (logrotate config included).
- Man pages `iowatchd(8)`, `iowatchd.conf(5)`, `iowatchd-log(1)`.
- Shell completions for bash, zsh and fish.
- `Makefile` with `PREFIX`, `DESTDIR`, `install` and `uninstall`;
  `./install.sh uninstall`.
- Packaging recipes: Arch (`PKGBUILD`), RPM spec, Debian (untested).
- Integration tests and GitHub Actions CI (fmt, clippy, tests).

### Changed
- Documentation, comments and messages are in English; MIT license.

## [0.1.0]

### Added
- Initial release: `/proc/diskstats` monitoring, per-process IO report above
  the threshold, journald and file logging, `device = auto`, systemd, OpenRC
  and runit service files.
