//! Command line parsing, configuration loading (defaults < file < CLI) and
//! `--check-config`.

use std::fs;

use crate::config::Config;
use crate::disk::{detect_device, read_stats};
use crate::report::parse_duration;

/// Configuration file read when `-c` is not given (if it exists).
const DEFAULT_CONFIG: &str = "/etc/iowatchd.conf";

/// Environment variable that selects the configuration file (like `-c`).
const CONFIG_ENV: &str = "IOWATCHD_CONFIG";

pub const USAGE: &str = "\
iowatchd - monitors disk IO usage and logs which processes are consuming it

Usage: iowatchd [OPTIONS]

Options:
  -c, --config FILE       configuration file (default: /etc/iowatchd.conf,
                          or $IOWATCHD_CONFIG); FILE.d/*.conf is read after it
  -d, --device DISK       disk to monitor, e.g. sda, nvme0n1 (default: auto)
  -t, --threshold PCT     IO usage % that triggers an alert (1-100)
  -i, --interval SEC      seconds between samples (minimum 1)
  -n, --top N             number of processes listed per alert
  -j, --json              log one JSON object per line
  -o, --once              take a single measurement, print it and exit
  -C, --check-config      validate the configuration, print it and exit
  -L, --list-devices      list the disks that can be monitored and exit
  -R, --report            summarize the alerts in the log file and exit
      --since PERIOD      period for --report: 90s, 30m, 24h, 7d (default: 24h)
  -f, --foreground        run in the foreground (the default; accepted for
                          compatibility with service managers)
  -q, --quiet             do not print to stdout (the log file still works)
  -v, --verbose           also print every sample to stdout
  -h, --help              show this help
  -V, --version           show the version

Short options can be grouped (-jq) and take attached values (-t80).
Use -- to end the options. Command-line options take precedence over the
configuration file.
SIGHUP reloads the configuration; SIGUSR1 logs the current status;
SIGTERM/SIGINT stop the program.
Exit codes: 0 success, 1 runtime error, 2 usage error.

Examples:
  iowatchd --once                 measure the root disk for one interval
  iowatchd -d nvme0n1 -t 60       monitor nvme0n1 with a 60% threshold
  iowatchd --check-config         validate /etc/iowatchd.conf
  iowatchd --list-devices         show the available disks
  iowatchd --report --since 7d    top IO offenders of the last week";

/// Text printed by `--version`.
pub fn version_text() -> String {
    format!(
        "iowatchd {}\nCopyright (c) Thiapalm. Licensed under the MIT license.\n{}",
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_REPOSITORY")
    )
}

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
    pub list_devices: bool,
    pub report: bool,
    /// Period of `--report` in seconds.
    pub since: Option<u64>,
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

/// Every long option name (without the leading `--`).
const LONG_OPTIONS: &[&str] = &[
    "help",
    "version",
    "once",
    "json",
    "check-config",
    "list-devices",
    "report",
    "since",
    "foreground",
    "quiet",
    "verbose",
    "config",
    "device",
    "threshold",
    "interval",
    "top",
];

/// Maps a short option letter to its long name.
fn long_for_short(c: char) -> Option<&'static str> {
    Some(match c {
        'h' => "help",
        'V' => "version",
        'o' => "once",
        'j' => "json",
        'C' => "check-config",
        'L' => "list-devices",
        'R' => "report",
        'f' => "foreground",
        'q' => "quiet",
        'v' => "verbose",
        'c' => "config",
        'd' => "device",
        't' => "threshold",
        'i' => "interval",
        'n' => "top",
        _ => return None,
    })
}

fn takes_value(long: &str) -> bool {
    matches!(
        long,
        "config" | "device" | "threshold" | "interval" | "top" | "since"
    )
}

/// Edit distance between two short strings (for "did you mean" hints).
fn edit_distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut cur = vec![i + 1];
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur.push((prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// Closest known long option to a mistyped one, if it is close enough.
fn suggest(name: &str) -> Option<&'static str> {
    LONG_OPTIONS
        .iter()
        .map(|&o| (edit_distance(name, o), o))
        .filter(|&(d, _)| d <= 2 && d < name.len())
        .min()
        .map(|(_, o)| o)
}

/// Applies one option to `cli`. `shown` is how the user typed it (for
/// messages). Returns `Some` for options that end parsing (help, version).
fn apply(
    cli: &mut Cli,
    long: &str,
    value: Option<String>,
    shown: &str,
) -> Result<Option<Parsed>, String> {
    let bad = |v: &str| format!("invalid value for '{shown}': '{v}'");
    let value = if takes_value(long) {
        Some(value.ok_or_else(|| format!("option '{shown}' requires a value"))?)
    } else if value.is_some() {
        return Err(format!("option '{shown}' does not take a value"));
    } else {
        None
    };

    match (long, value) {
        ("help", _) => return Ok(Some(Parsed::Help)),
        ("version", _) => return Ok(Some(Parsed::Version)),
        ("once", _) => cli.once = true,
        ("json", _) => cli.json = true,
        ("check-config", _) => cli.check_config = true,
        ("list-devices", _) => cli.list_devices = true,
        ("report", _) => cli.report = true,
        ("since", Some(v)) => cli.since = Some(parse_duration(&v).ok_or_else(|| bad(&v))?),
        ("foreground", _) => {}
        ("quiet", _) => cli.quiet = true,
        ("verbose", _) => cli.verbose = true,
        ("config", Some(v)) => cli.config = Some(v),
        ("device", Some(v)) => cli.device = Some(v),
        ("threshold", Some(v)) => {
            let t: f64 = v.parse().map_err(|_| bad(&v))?;
            if !(t > 0.0 && t <= 100.0) {
                return Err(bad(&v));
            }
            cli.threshold = Some(t);
        }
        ("interval", Some(v)) => {
            cli.interval = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
        }
        ("top", Some(v)) => {
            cli.top_n = Some(v.parse().ok().filter(|&n| n >= 1).ok_or_else(|| bad(&v))?);
        }
        _ => unreachable!("option table out of sync: {long}"),
    }
    Ok(None)
}

impl Cli {
    /// Parses the arguments (without the program name). Accepts
    /// `--option value`, `--option=value`, `-o value`, `-ovalue`, grouped
    /// short flags (`-jq`) and `--` to end the options. Errors are
    /// ready-to-display messages (exit code 2).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
        let mut cli = Cli::default();
        let mut args = args.into_iter();

        while let Some(arg) = args.next() {
            if arg == "--" {
                // No positional arguments exist, so anything after `--` is an error.
                return match args.next() {
                    Some(extra) => Err(format!("unexpected argument '{extra}'")),
                    None => Ok(Parsed::Run(cli)),
                };
            }

            if let Some(rest) = arg.strip_prefix("--") {
                let (name, inline) = match rest.split_once('=') {
                    Some((n, v)) => (n, Some(v.to_string())),
                    None => (rest, None),
                };
                let Some(&long) = LONG_OPTIONS.iter().find(|&&o| o == name) else {
                    return Err(match suggest(name) {
                        Some(s) => format!("unknown option '--{name}' (did you mean '--{s}'?)"),
                        None => format!("unknown option '--{name}'"),
                    });
                };
                let value = match inline {
                    None if takes_value(long) => args.next(),
                    other => other,
                };
                if let Some(done) = apply(&mut cli, long, value, &format!("--{long}"))? {
                    return Ok(done);
                }
            } else if arg.len() > 1 && arg.starts_with('-') {
                // A cluster of short options; a value option consumes the
                // rest of the cluster (`-t80`) or the next argument.
                let letters: Vec<char> = arg[1..].chars().collect();
                for (i, &c) in letters.iter().enumerate() {
                    let Some(long) = long_for_short(c) else {
                        return Err(format!("unknown option '-{c}'"));
                    };
                    let shown = format!("-{c}");
                    if takes_value(long) {
                        let attached: String = letters[i + 1..].iter().collect();
                        let value = if attached.is_empty() {
                            args.next()
                        } else {
                            Some(attached)
                        };
                        if let Some(done) = apply(&mut cli, long, value, &shown)? {
                            return Ok(done);
                        }
                        break;
                    }
                    if let Some(done) = apply(&mut cli, long, None, &shown)? {
                        return Ok(done);
                    }
                }
            } else {
                return Err(format!("unexpected argument '{arg}'"));
            }
        }
        Ok(Parsed::Run(cli))
    }
}

/// Path of the configuration file and whether it was chosen explicitly
/// (`-c` or `$IOWATCHD_CONFIG`), in which case it must exist.
fn config_path(cli: &Cli) -> (String, bool) {
    if let Some(p) = &cli.config {
        return (p.clone(), true);
    }
    match std::env::var(CONFIG_ENV) {
        Ok(p) if !p.is_empty() => (p, true),
        _ => (DEFAULT_CONFIG.to_string(), false),
    }
}

/// Drop-in files `<config>.d/*.conf`, in alphabetical order.
fn dropins(path: &str) -> Vec<String> {
    let mut files: Vec<String> = fs::read_dir(format!("{path}.d"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "conf"))
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    files.sort();
    files
}

/// Builds the final configuration: defaults < file < drop-ins < command
/// line, and resolves `device = auto`. Returns the configuration together
/// with the warnings found while parsing the files. Also used when
/// reloading (SIGHUP).
/// Reads the configuration file and its drop-ins on top of the defaults.
fn load_files(cli: &Cli) -> Result<(Config, Vec<String>), String> {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();

    // An explicit file is mandatory; the default one is used only if it exists.
    let (path, explicit) = config_path(cli);
    let mut sources = vec![path.clone()];
    sources.extend(dropins(&path));
    for (n, file) in sources.iter().enumerate() {
        match fs::read_to_string(file) {
            Ok(text) => {
                warnings.extend(cfg.apply(&text).into_iter().map(|w| format!("{file}: {w}")));
            }
            Err(e) if n == 0 && explicit => return Err(format!("could not read {file}: {e}")),
            Err(e) if n > 0 => warnings.push(format!("{file}: could not read: {e}")),
            Err(_) => {}
        }
    }
    Ok((cfg, warnings))
}

/// `--report` needs only the files' settings (log file, top N), not a disk.
pub fn report_settings(cli: &Cli) -> Result<Config, String> {
    let (mut cfg, warnings) = load_files(cli)?;
    for w in warnings {
        eprintln!("iowatchd: configuration warning: {w}");
    }
    if let Some(n) = cli.top_n {
        cfg.top_n = n;
    }
    Ok(cfg)
}

fn load_config(cli: &Cli) -> Result<(Config, Vec<String>), String> {
    let (mut cfg, warnings) = load_files(cli)?;

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
    if read_stats(&cfg.device).is_none() {
        return Err(format!(
            "device '{}' not found in /proc/diskstats (see --list-devices)",
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
            println!(
                "notify_command={}",
                cfg.notify_command.as_deref().unwrap_or("")
            );
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

    fn run(v: &[&str]) -> Cli {
        match parse(v) {
            Ok(Parsed::Run(c)) => c,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn accepts_flags() {
        let c = run(&["-j", "-q", "-v", "-C", "-f", "-L"]);
        assert!(c.json && c.quiet && c.verbose && c.check_config && c.list_devices);
    }

    #[test]
    fn accepts_long_short_and_equals_forms() {
        assert_eq!(parse(&["-h"]), Ok(Parsed::Help));
        assert_eq!(parse(&["--version"]), Ok(Parsed::Version));
        let c = run(&["-d", "nvme0n1", "--threshold=60", "-i", "5", "--once"]);
        assert_eq!(c.device.as_deref(), Some("nvme0n1"));
        assert_eq!(c.threshold, Some(60.0));
        assert_eq!(c.interval, Some(5));
        assert!(c.once);
    }

    #[test]
    fn accepts_grouped_flags_and_attached_values() {
        let c = run(&["-jqv"]);
        assert!(c.json && c.quiet && c.verbose);
        let c = run(&["-t80", "-oi3", "-dsda"]);
        assert_eq!(c.threshold, Some(80.0));
        assert_eq!(c.interval, Some(3));
        assert_eq!(c.device.as_deref(), Some("sda"));
        assert!(c.once);
        // A value option inside a cluster takes the next argument.
        let c = run(&["-jt", "70"]);
        assert!(c.json);
        assert_eq!(c.threshold, Some(70.0));
    }

    #[test]
    fn double_dash_ends_options() {
        assert_eq!(parse(&["--"]), Ok(Parsed::Run(Cli::default())));
        assert!(parse(&["-j", "--", "x"]).is_err());
    }

    #[test]
    fn suggests_similar_options() {
        let e = parse(&["--thresold=5"]).unwrap_err();
        assert!(e.contains("did you mean '--threshold'"), "{e}");
        let e = parse(&["--zzzzzz"]).unwrap_err();
        assert!(!e.contains("did you mean"), "{e}");
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
        assert!(parse(&["-x"]).is_err());
        assert!(parse(&["--json=1"]).is_err());
    }

    #[test]
    fn edit_distance_counts_edits() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("threshold", "treshold"), 1);
    }

    #[test]
    fn dropins_are_sorted_and_filtered() {
        let dir = std::env::temp_dir().join(format!("iowatchd-dropins-{}", std::process::id()));
        let d = dir.join("main.conf.d");
        fs::create_dir_all(&d).unwrap();
        for f in ["20-b.conf", "10-a.conf", "ignored.txt"] {
            fs::write(d.join(f), "").unwrap();
        }
        let found = dropins(dir.join("main.conf").to_str().unwrap());
        let names: Vec<_> = found
            .iter()
            .map(|p| p.rsplit('/').next().unwrap().to_string())
            .collect();
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(names, ["10-a.conf", "20-b.conf"]);
        assert!(dropins("/nonexistent/iowatchd.conf").is_empty());
    }

    #[test]
    fn explicit_config_path_wins() {
        let cli = run(&["-c", "/tmp/x.conf"]);
        assert_eq!(config_path(&cli), ("/tmp/x.conf".to_string(), true));
    }
}
