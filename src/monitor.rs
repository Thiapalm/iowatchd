//! The monitoring loop: samples the disk, tracks the normal/alert state with
//! hysteresis and triggers the per-process report.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::cli::{build_config, Cli};
use crate::config::Config;
use crate::disk::{read_io_ticks, utilization};
use crate::logger::{json_str, Logger};
use crate::notify::sd_notify;
use crate::procs::report_top_processes;
use crate::signals::{nap, RELOAD, STOP};

/// Monitor state: IO normal or in alert.
enum State {
    Normal { consecutive_high: u32 },
    Alert { last_report: Instant },
}

fn make_logger(cfg: &Config, cli: &Cli) -> Logger {
    Logger {
        file: cfg.log_file.clone(),
        max_bytes: cfg.log_max_bytes,
        json: cfg.log_json,
        quiet: cli.quiet,
    }
}

/// `--once` mode: measures for one interval, prints, and lists the processes
/// if above the threshold. Does not write to the log file.
pub fn run_once(cfg: &Config, cli: &Cli) {
    let logger = Logger {
        file: None,
        max_bytes: 0,
        json: cfg.log_json,
        quiet: cli.quiet,
    };
    let start_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
    let start = Instant::now();
    nap(cfg.interval);
    let ticks = read_io_ticks(&cfg.device).unwrap_or(start_ticks);
    let util = utilization(
        ticks.saturating_sub(start_ticks),
        start.elapsed().as_millis() as u64,
    );
    if util >= cfg.threshold {
        report_top_processes(cfg, &logger, util);
    } else {
        logger.emit(
            "sample",
            &format!("IO={util:.0}% on {}", cfg.device),
            &format!("\"device\":{},\"util\":{util:.0}", json_str(&cfg.device)),
        );
    }
}

/// Runs the daemon loop until SIGINT/SIGTERM. SIGHUP re-reads the configuration.
pub fn run(mut cfg: Config, cli: &Cli) {
    let mut logger = make_logger(&cfg, cli);
    let mut prev_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
    let mut prev_time = Instant::now();
    let mut state = State::Normal {
        consecutive_high: 0,
    };

    logger.log(
        "started",
        &format!(
            "started: device={} threshold={}% interval={}s",
            cfg.device, cfg.threshold, cfg.interval
        ),
    );
    // Tell systemd (Type=notify) that startup is complete.
    sd_notify(&format!("READY=1\nSTATUS=monitoring {}", cfg.device));

    loop {
        nap(cfg.interval);

        if STOP.load(Ordering::SeqCst) {
            sd_notify("STOPPING=1");
            logger.log("stopped", "stopped");
            break;
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            match build_config(cli) {
                Ok(new) => {
                    cfg = new;
                    logger = make_logger(&cfg, cli);
                    // The disk may have changed: reset the references and the state.
                    prev_ticks = read_io_ticks(&cfg.device).unwrap_or(0);
                    prev_time = Instant::now();
                    state = State::Normal {
                        consecutive_high: 0,
                    };
                    sd_notify(&format!("STATUS=monitoring {}", cfg.device));
                    logger.log(
                        "reloaded",
                        &format!(
                            "configuration reloaded: device={} threshold={}% interval={}s",
                            cfg.device, cfg.threshold, cfg.interval
                        ),
                    );
                }
                Err(e) => logger.log(
                    "reload_failed",
                    &format!("reload failed ({e}); keeping the current configuration"),
                ),
            }
            continue;
        }

        let Some(ticks) = read_io_ticks(&cfg.device) else {
            eprintln!("warning: failed to read /proc/diskstats");
            continue;
        };
        let now = Instant::now();
        let util = utilization(
            ticks.saturating_sub(prev_ticks),
            now.duration_since(prev_time).as_millis() as u64,
        );
        prev_ticks = ticks;
        prev_time = now;

        if cli.verbose && !cli.quiet {
            println!("sample: IO={util:.0}% on {}", cfg.device);
        }

        state = match state {
            State::Normal { consecutive_high } => {
                if util >= cfg.threshold {
                    let count = consecutive_high + 1;
                    if count >= cfg.trigger_samples {
                        report_top_processes(&cfg, &logger, util);
                        // The process measurement took time; reset the
                        // reference for the next disk sample.
                        prev_ticks = read_io_ticks(&cfg.device).unwrap_or(prev_ticks);
                        prev_time = Instant::now();
                        State::Alert {
                            last_report: Instant::now(),
                        }
                    } else {
                        State::Normal {
                            consecutive_high: count,
                        }
                    }
                } else {
                    State::Normal {
                        consecutive_high: 0,
                    }
                }
            }
            State::Alert { last_report } => {
                if util < cfg.clear_threshold {
                    logger.emit(
                        "normal",
                        &format!("IO back to normal: {util:.0}% on {}", cfg.device),
                        &format!("\"device\":{},\"util\":{util:.0}", json_str(&cfg.device)),
                    );
                    State::Normal {
                        consecutive_high: 0,
                    }
                } else if util >= cfg.threshold
                    && last_report.elapsed() >= Duration::from_secs(cfg.cooldown)
                {
                    report_top_processes(&cfg, &logger, util);
                    prev_ticks = read_io_ticks(&cfg.device).unwrap_or(prev_ticks);
                    prev_time = Instant::now();
                    State::Alert {
                        last_report: Instant::now(),
                    }
                } else {
                    State::Alert { last_report }
                }
            }
        };
    }
}
