//! The monitoring loop: samples the disk, tracks the normal/alert state with
//! hysteresis and triggers the per-process report.

use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use crate::cli::{build_config, Cli};
use crate::config::Config;
use crate::disk::{metrics, read_stats, DiskStats, Metrics};
use crate::hook::{run_hook, HookEvent};
use crate::logger::{json_str, Logger};
use crate::notify::sd_notify;
use crate::procs::{report_top_processes, sample_text, unprivileged_warning};
use crate::signals::{nap, RELOAD, STATUS, STOP};

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
    let start_stats = read_stats(&cfg.device).unwrap_or_default();
    if let Some(w) = unprivileged_warning() {
        eprintln!("iowatchd: warning: {w}");
    }
    let start = Instant::now();
    nap(cfg.interval);
    let stats = read_stats(&cfg.device).unwrap_or(start_stats);
    let m = metrics(&start_stats, &stats, start.elapsed().as_millis() as u64);
    if m.util >= cfg.threshold {
        report_top_processes(cfg, &logger, &m);
    } else {
        log_sample(&logger, cfg, &m);
    }
}

/// Logs a plain sample: utilization plus the other disk metrics.
fn log_sample(logger: &Logger, cfg: &Config, m: &Metrics) {
    let details = sample_text(m);
    logger.emit(
        "sample",
        &format!("IO={:.0}% on {}{details}", m.util, cfg.device),
        &format!(
            "\"device\":{},\"util\":{:.0},\"queue_depth\":{:.2}{}",
            json_str(&cfg.device),
            m.util,
            m.queue_depth,
            m.await_ms
                .map(|a| format!(",\"await_ms\":{a:.2}"))
                .unwrap_or_default()
        ),
    );
}

/// Runs the configured hook, if any.
fn notify(cfg: &Config, event: &str, util: f64, message: &str) {
    if let Some(cmd) = &cfg.notify_command {
        run_hook(
            cmd,
            &HookEvent {
                event,
                device: &cfg.device,
                util,
                message,
            },
        );
    }
}

/// Runs the daemon loop until SIGINT/SIGTERM. SIGHUP re-reads the configuration.
pub fn run(mut cfg: Config, cli: &Cli) {
    let mut logger = make_logger(&cfg, cli);
    let mut prev: DiskStats = read_stats(&cfg.device).unwrap_or_default();
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
    if let Some(w) = unprivileged_warning() {
        logger.log("warning", &format!("warning: {w}"));
    }
    let mut last_util = 0.0;

    loop {
        nap(cfg.interval);

        if STOP.load(Ordering::SeqCst) {
            sd_notify("STOPPING=1");
            logger.log("stopped", "stopped");
            break;
        }

        // SIGUSR1: report the current state. The sample is skipped; the next
        // one simply covers a longer period.
        if STATUS.swap(false, Ordering::SeqCst) {
            let name = match state {
                State::Normal { .. } => "normal",
                State::Alert { .. } => "alert",
            };
            logger.emit(
                "status",
                &format!(
                    "status: device={} IO={last_util:.0}% state={name} threshold={}% interval={}s",
                    cfg.device, cfg.threshold, cfg.interval
                ),
                &format!(
                    "\"device\":{},\"util\":{last_util:.0},\"state\":\"{name}\"",
                    json_str(&cfg.device)
                ),
            );
            continue;
        }

        if RELOAD.swap(false, Ordering::SeqCst) {
            match build_config(cli) {
                Ok(new) => {
                    cfg = new;
                    logger = make_logger(&cfg, cli);
                    // The disk may have changed: reset the references and the state.
                    prev = read_stats(&cfg.device).unwrap_or_default();
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

        let Some(stats) = read_stats(&cfg.device) else {
            eprintln!("warning: failed to read /proc/diskstats");
            continue;
        };
        let now = Instant::now();
        let m = metrics(
            &prev,
            &stats,
            now.duration_since(prev_time).as_millis() as u64,
        );
        let util = m.util;
        prev = stats;
        prev_time = now;
        last_util = util;

        if cli.verbose && !cli.quiet {
            println!("sample: IO={util:.0}% on {}{}", cfg.device, sample_text(&m));
        }

        state = match state {
            State::Normal { consecutive_high } => {
                if util >= cfg.threshold {
                    let count = consecutive_high + 1;
                    if count >= cfg.trigger_samples {
                        let msg = report_top_processes(&cfg, &logger, &m);
                        notify(&cfg, "alert", util, &msg);
                        // The process measurement took time; reset the
                        // reference for the next disk sample.
                        prev = read_stats(&cfg.device).unwrap_or(prev);
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
                    let msg = format!("IO back to normal: {util:.0}% on {}", cfg.device);
                    logger.emit(
                        "normal",
                        &msg,
                        &format!("\"device\":{},\"util\":{util:.0}", json_str(&cfg.device)),
                    );
                    notify(&cfg, "normal", util, &msg);
                    State::Normal {
                        consecutive_high: 0,
                    }
                } else if util >= cfg.threshold
                    && last_report.elapsed() >= Duration::from_secs(cfg.cooldown)
                {
                    let msg = report_top_processes(&cfg, &logger, &m);
                    notify(&cfg, "alert", util, &msg);
                    prev = read_stats(&cfg.device).unwrap_or(prev);
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
