//! iowatchd
//!
//! Monitors the IO utilization of a disk (via `/proc/diskstats`) and, when it
//! goes above the configured threshold, logs which processes are generating
//! the most reads/writes (via `/proc/<pid>/io`).
//!
//! Design principles (minimal resource usage):
//! * No external dependencies and no async runtime.
//! * The main loop reads a single small file and sleeps (`nap`) between
//!   samples: practically zero CPU.
//! * The process scan, which is the "expensive" part, only happens when the
//!   threshold is exceeded.
//!
//! Module layout:
//! * [`cli`]     argument parsing and configuration loading
//! * [`config`]  configuration values and file parser
//! * [`disk`]    disk detection and utilization
//! * [`procs`]   per-process IO and the alert report
//! * [`logger`]  stdout/file logging (text or JSON)
//! * [`monitor`] the sampling loop and alert state machine
//! * [`signals`] signal handlers and interruptible sleep
//! * [`notify`]  systemd readiness notification

mod cli;
mod config;
mod disk;
mod logger;
mod monitor;
mod notify;
mod procs;
mod signals;

use cli::{build_config, check_config, Cli, Parsed, USAGE};

fn main() {
    let cli = match Cli::parse(std::env::args().skip(1)) {
        Ok(Parsed::Help) => return println!("{USAGE}"),
        Ok(Parsed::Version) => return println!("iowatchd {}", env!("CARGO_PKG_VERSION")),
        Ok(Parsed::Run(cli)) => cli,
        Err(e) => {
            eprintln!("iowatchd: {e}\nTry 'iowatchd --help' for more information.");
            std::process::exit(2);
        }
    };

    if cli.check_config {
        std::process::exit(check_config(&cli));
    }

    let cfg = build_config(&cli).unwrap_or_else(|e| {
        eprintln!("iowatchd: error: {e}");
        std::process::exit(1);
    });

    signals::install_signal_handlers();

    if cli.once {
        monitor::run_once(&cfg, &cli);
    } else {
        monitor::run(cfg, &cli);
    }
}
