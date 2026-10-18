# Copilot instructions for iowatchd

Linux daemon (pure Rust, **zero crate dependencies**) that samples `/proc/diskstats`, and when `%util` crosses a threshold logs the processes doing the most IO (from `/proc/<pid>/io`). Keep it dependency-free and near-zero CPU: the cheap sampling loop runs constantly; the expensive per-process scan runs only during an alert.

## Build, test, lint

CI (`.github/workflows/ci.yml`) runs with `RUSTFLAGS=-D warnings` and `--locked`:

    cargo fmt --check
    cargo clippy --all-targets --locked
    cargo test --locked
    make                      # release build; `make install DESTDIR=... PREFIX=/usr` is smoke-tested
    shellcheck -S warning iowatchd-log iowatchd-notify

Single test: `cargo test --locked <name>` (unit tests live in each `src/*.rs` under `#[cfg(test)]`; integration tests in `tests/cli.rs` run the real binary via `CARGO_BIN_EXE_iowatchd`, check exit codes/signals, and skip themselves when no block device exists).

## Architecture

`src/main.rs` documents the module layout. The flow: `cli` (argument parsing + layered config loading) → `config` (`key = value` parser; defaults in `Config::default`) → `monitor` (sampling loop and alert state machine with hysteresis: `threshold`/`clear_threshold`, `trigger_samples`, `cooldown`) → `disk` (device detection for `device = auto`, `%util`, await/queue/PSI) → `procs` (per-process IO scan) → `logger` (stdout/journald + file, text or JSON, built-in rotation to `<file>.1`) and `hook` (`notify_command`).

`--report` (`src/report.rs`) is the post-hoc analyzer: it **re-parses the log text** (both text and JSON formats, plus the rotated `.1`), so `parse_alert`/`parse_hit` must stay in sync with the alert line format produced by `procs`/`logger`. It derives a diagnosis from the header metrics (`await`, throughput, `psi`): high latency with little traffic = slow/degrading disk, not a busy one. It optionally shells out to `smartctl -A` (config `smart = auto|off`, needs root), only in `--report`, never in the daemon loop.

Config precedence: CLI flags > `/etc/iowatchd.conf.d/*.conf` drop-ins > `/etc/iowatchd.conf` (path may come from `$IOWATCHD_CONFIG`). `SIGHUP` reloads, `SIGUSR1` logs status. Exit codes: 0 ok, 1 runtime error, 2 usage error.

## Conventions

- Adding a config key or CLI option touches several places that must stay consistent: `src/config.rs`/`src/cli.rs` (+ `--check-config` output), `config.conf`, `man/iowatchd.conf.5` / `man/iowatchd.8`, `completions/` (bash, zsh, fish), README, and `CHANGELOG.md` (Keep a Changelog, `[Unreleased]` section).
- Invalid config lines are collected as warnings, not fatal errors; `Config::normalize` clamps unsafe values.
- Shell helpers (`iowatchd-log`, `iowatchd-notify`) and completions are syntax-checked in CI (bash/zsh) and shellchecked.
- Code comments are sparse and in English; user-facing output is English.

## Releasing

Tag-driven: pushing `vX.Y.Z` triggers `.github/workflows/release.yml` (static musl binaries for x86_64 and aarch64, GitHub release, `cargo publish` if `CARGO_REGISTRY_TOKEN` is set). Release commit "Release X.Y.Z" bumps the version in `Cargo.toml`, `Cargo.lock`, the `.TH` line of each `man/*`, `packaging/arch/PKGBUILD`, `packaging/rpm/iowatchd.spec` (+ changelog entry), `packaging/debian/changelog`, and turns `[Unreleased]` into the dated version in `CHANGELOG.md`; `scripts/check-versions.sh [vX.Y.Z]` verifies they all agree (CI and the release workflow run it). After the workflow finishes, a follow-up commit points `packaging/arch-bin/PKGBUILD` and `packaging/homebrew/iowatchd.rb` at the new version with the sha256 values from the release's `.sha256` assets.

## Package checks

CI builds and tests every package: `.deb`/`.rpm` (install + lint), Arch `PKGBUILD` and `-bin` (`makepkg`, the latter against the published release), the Homebrew formula (`brew audit/install/test`), and the static musl tarballs for x86_64/aarch64 (`scripts/make-tarball.sh`, `scripts/check-tarball.sh`). Scripts in `scripts/` are shellchecked.
