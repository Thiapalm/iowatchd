//! Command line parsing, configuration loading (defaults < file < CLI) and
//! `--check-config`.

use std::fs;

use crate::config::Config;
use crate::disk::{detect_device, read_io_ticks};

/// Configuration file read when `-c` is not given (if it exists).
const DEFAULT_CONFIG: &str = "/etc/iowatchd.conf";

pub const USAGE: &str = "\
iowatchd - monitors disk IO usage and logs which processes are consuming it

Usage: iowatchd [OPTIONS]

Options:
  -c, --config FILE       configuration file (default: /etc/iowatchd.conf)
  -d, --device DISK       disk to monitor, e.g. sda, nvme0n1 (default: auto)
  -t, --threshold PCT     IO usage % that triggers an alert (1-100)
  -i, --interval SEC      seconds between samples (minimum 1)
  -n, --top N             number of processes listed per alert
  -j, --json              log one JSON object per line
  -o, --once              take a single measurement, print it and exit
  -C, --check-config      validate the configuration, print it and exit
  -f, --foreground        run in the foreground (the default; accepted for
                          compatibility with service managers)
  -q, --quiet             do not print to stdout (the log file still works)
  -v, --verbose           also print every sample to stdout
  -h, --help              show this help
  -V, --version           show the version

Command-line options take precedence over the configuration file.
SIGHUP reloads the configuration; SIGTERM/SIGINT stop the program.
Exit codes: 0 success, 1 runtime error, 2 usage error.

Examples:
  iowatchd --once                 measure the root disk for one interval
  iowatchd -d nvme0n1 -t 60       monitor nvme0n1 with a 60% threshold
  iowatchd --check-config         validate /etc/iowatchd.conf";

/// Options given on the command line; `None` means "not provided".
#[derive(Debug, Default, PartialEq)]
pub struct Cli {
    pub config: Option<String>,
    pub device: Option<String>,
    pub threshold: Option<f64>,
    pub interval: Option<u64>,
    pub top_n: Option<usize>,
    pub json: bool,
    pub once: bool,
    pub check_config: bool,
    pub quiet: bool,
    pub verbose: bool,
}

/// Result of parsing the arguments.
#[derive(Debug, PartialEq)]
pub enum Parsed {
    Help,
    Version,
    Run(Cli),
}

impl Cli {
    /// Parses the arguments (without the program name). Accepts
    /// `--option value`, `--option=value` and `-o value`. Errors are
    /// ready-to-display messages (exit code 2).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
        let mut cli = Cli::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            // Split `--option=value` into name and embedded value.
            let (name, inline) = match arg.split_once('=') {
                Some((n, v)) if n.starts_with("--") => (n.to_string(), Some(v.to_string())),
                _ => (arg.clone(), None),
            };

            // Option value: the embedded one or the next argument.
            let value = |args: &mut dyn Iterator<Item = String>| -> Result<String, String> {
                inline
                    .clone()
                    .or_else(|| args.next())
                    .ok_or_else(|| format!("option '{name}' requires a value"))
            };
            let bad = |v: &str| format!("invalid value for '{name}': '{v}'");

            match name.as_str() {
                "-h" | "--help" => return Ok(Parsed::Help),
                "-V" | "--version" => return Ok(Parsed::Version),
                "-o" | "--once" => cli.once = true,
                "-j" | "--json" => cli.json = true,
                "-C" | "--check-config" => cli.check_config = true,
                "-f" | "--foreground" => {}
                "-q" | "--quiet" => cli.quiet = true,
                "-v" | "--verbose" => cli.verbose = true,
                "-c" | "--config" => cli.config = Some(value(&mut args)?),
                "-d" | "--device" => cli.device = Some(value(&mut args)?),
                "-t" | "--threshold" => {
                    let v = value(&mut args)?;
                    let t: f64 = v.parse().map_err(|_| bad(&v))?;
                    if !(t > 0.0 && t <= 100.0) {
                        return Err(bad(&v));
                    }
                    cli.threshold = Some(t);
                }
                "-i" | "--interval" => {
                    let v = value(&mut args)?;
                    cli.interval = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
                }
                "-n" | "--top" => {
                    let v = value(&mut args)?;
                    cli.top_n = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
                }
                other if other.starts_with('-') => return Err(format!("unknown option '{other}'")),
                other => return Err(format!("unexpected argument '{other}'")),
            }
        }
        Ok(Parsed::Run(cli))
    }
}

/// Builds the final configuration: defaults < file < command line, and
/// resolves `device = auto`. Returns the configuration together with the
/// warnings found while parsing the file. Also used when reloading (SIGHUP).
fn load_config(cli: &Cli) -> Result<(Config, Vec<String>), String> {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();

    // With -c the file is mandatory; without it, use the default if it exists.
    let path = cli.config.as_deref().unwrap_or(DEFAULT_CONFIG);
    match fs::read_to_string(path) {
        Ok(text) => {
            let (parsed, found) = Config::parse(&text);
            warnings = found.into_iter().map(|w| format!("{path}: {w}")).collect();
            cfg = parsed;
        }
        Err(e) if cli.config.is_some() => {
            return Err(format!("could not read {path}: {e}"));
        }
        Err(_) => {}
    }

    if let Some(d) = &cli.device {
        cfg.device = d.clone();
    }
    if let Some(t) = cli.threshold {
        cfg.threshold = t;
    }
    if let Some(i) = cli.interval {
        cfg.interval = i;
    }
    if let Some(n) = cli.top_n {
        cfg.top_n = n;
    }
    if cli.json {
        cfg.log_json = true;
    }
    cfg.normalize();

    if cfg.device == "auto" {
        cfg.device = detect_device().ok_or(
            "could not detect the disk; use --device or set 'device' in the configuration",
        )?;
    }
    if read_io_ticks(&cfg.device).is_none() {
        return Err(format!(
            "device '{}' not found in /proc/diskstats",
            cfg.device
        ));
    }
    Ok((cfg, warnings))
}

/// Like [`load_config`], but prints the configuration warnings to stderr.
pub fn build_config(cli: &Cli) -> Result<Config, String> {
    let (cfg, warnings) = load_config(cli)?;
    for w in warnings {
        eprintln!("iowatchd: configuration warning: {w}");
    }
    Ok(cfg)
}

/// `--check-config`: validates the configuration and prints the effective
/// values. Returns the process exit code (1 if there are errors or warnings).
pub fn check_config(cli: &Cli) -> i32 {
    match load_config(cli) {
        Ok((cfg, warnings)) => {
            for w in &warnings {
                eprintln!("iowatchd: configuration warning: {w}");
            }
            println!("device={}", cfg.device);
            println!("threshold={}", cfg.threshold);
            println!("clear_threshold={}", cfg.clear_threshold);
            println!("interval={}", cfg.interval);
            println!("trigger_samples={}", cfg.trigger_samples);
            println!("process_window={}", cfg.process_window);
            println!("top_n={}", cfg.top_n);
            println!("cooldown={}", cfg.cooldown);
            match &cfg.log_file {
                Some(p) => println!("log_file={}", p.display()),
                None => println!("log_file="),
            }
            println!("log_max_mb={}", cfg.log_max_bytes / 1024 / 1024);
            println!("log_format={}", if cfg.log_json { "json" } else { "text" });
            if warnings.is_empty() {
                println!("configuration OK");
                0
            } else {
                1
            }
        }
        Err(e) => {
            eprintln!("iowatchd: error: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &[&str]) -> Result<Parsed, String> {
        Cli::parse(v.iter().map(|s| s.to_string()))
    }

    #[test]
    fn accepts_new_flags() {
        let Ok(Parsed::Run(c)) = parse(&["-j", "-q", "-v", "-C", "-f"]) else {
            panic!("parse failed")
        };
        assert!(c.json && c.quiet && c.verbose && c.check_config);
    }

    #[test]
    fn accepts_long_short_and_equals_forms() {
        assert_eq!(parse(&["-h"]), Ok(Parsed::Help));
        assert_eq!(parse(&["--version"]), Ok(Parsed::Version));
        let Ok(Parsed::Run(c)) = parse(&["-d", "nvme0n1", "--threshold=60", "-i", "5", "--once"])
        else {
            panic!("parse failed")
        };
        assert_eq!(c.device.as_deref(), Some("nvme0n1"));
        assert_eq!(c.threshold, Some(60.0));
        assert_eq!(c.interval, Some(5));
        assert!(c.once);
    }

    #[test]
    fn rejects_invalid_input() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["file"]).is_err());
        assert!(parse(&["-t"]).is_err());
        assert!(parse(&["-t", "0"]).is_err());
        assert!(parse(&["-t", "101"]).is_err());
        assert!(parse(&["-i", "0"]).is_err());
        assert!(parse(&["-n", "abc"]).is_err());
    }
}
