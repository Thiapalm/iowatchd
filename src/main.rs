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

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Monitor parameters. Defaults are in [`Config::default`]; they can be
/// overridden by a `key = value` file (see `config.conf`).
#[derive(Debug, Clone, PartialEq)]
struct Config {
    /// Device name in `/proc/diskstats` (e.g. `sda`, `nvme0n1`) or `auto`
    /// to detect the physical disk that holds the root partition.
    device: String,
    /// IO utilization (%) at or above which processes are logged.
    threshold: f64,
    /// IO utilization (%) below which IO is considered back to normal.
    /// Keeping it below `threshold` avoids flapping between states.
    clear_threshold: f64,
    /// Seconds between disk samples.
    interval: u64,
    /// Consecutive samples above the threshold required to trigger.
    trigger_samples: u32,
    /// Window (s) used to measure per-process IO once triggered.
    process_window: u64,
    /// How many processes to list per report.
    top_n: usize,
    /// Minimum interval (s) between reports while IO stays high.
    cooldown: u64,
    /// Log file in addition to stdout/journald (`None` disables it).
    log_file: Option<PathBuf>,
    /// Maximum log file size (bytes) before rotating; 0 disables the built-in
    /// rotation (useful when logrotate manages the file).
    log_max_bytes: u64,
    /// Log format: plain text or one JSON object per line.
    log_json: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: "auto".into(),
            threshold: 80.0,
            clear_threshold: 70.0,
            interval: 2,
            trigger_samples: 2,
            process_window: 1,
            top_n: 5,
            cooldown: 10,
            log_file: Some(PathBuf::from("/var/log/iowatchd.log")),
            log_max_bytes: 5 * 1024 * 1024,
            log_json: false,
        }
    }
}

impl Config {
    /// Applies the contents of a configuration file on top of the defaults.
    /// Invalid lines are ignored and returned as warnings.
    fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();

        for (n, raw) in text.lines().enumerate() {
            // Strip comments and whitespace.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("line {}: expected 'key = value'", n + 1));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());

            let ok = match key {
                "device" => {
                    cfg.device = value.to_string();
                    !value.is_empty()
                }
                "threshold" => value.parse().map(|v| cfg.threshold = v).is_ok(),
                "clear_threshold" => value.parse().map(|v| cfg.clear_threshold = v).is_ok(),
                "interval" => value.parse().map(|v| cfg.interval = v).is_ok(),
                "trigger_samples" => value.parse().map(|v| cfg.trigger_samples = v).is_ok(),
                "process_window" => value.parse().map(|v| cfg.process_window = v).is_ok(),
                "top_n" => value.parse().map(|v| cfg.top_n = v).is_ok(),
                "cooldown" => value.parse().map(|v| cfg.cooldown = v).is_ok(),
                "log_file" => {
                    cfg.log_file = if value.is_empty() {
                        None
                    } else {
                        Some(value.into())
                    };
                    true
                }
                "log_max_mb" => value
                    .parse::<u64>()
                    .map(|v| cfg.log_max_bytes = v * 1024 * 1024)
                    .is_ok(),
                "log_format" => match value {
                    "text" => {
                        cfg.log_json = false;
                        true
                    }
                    "json" => {
                        cfg.log_json = true;
                        true
                    }
                    _ => false,
                },
                _ => {
                    warnings.push(format!("line {}: unknown key '{}'", n + 1, key));
                    true
                }
            };
            if !ok {
                warnings.push(format!("line {}: invalid value for '{}'", n + 1, key));
            }
        }

        cfg.normalize();
        (cfg, warnings)
    }

    /// Enforces safe values (avoids a loop without sleeping or inverted hysteresis).
    fn normalize(&mut self) {
        self.interval = self.interval.max(1);
        self.process_window = self.process_window.max(1);
        self.trigger_samples = self.trigger_samples.max(1);
        if self.clear_threshold > self.threshold {
            self.clear_threshold = self.threshold;
        }
    }
}

// ---------------------------------------------------------------------------
// Logging (stdout/journald + file with simple rotation)
// ---------------------------------------------------------------------------

struct Logger {
    file: Option<PathBuf>,
    max_bytes: u64,
    /// Emit one JSON object per line instead of plain text.
    json: bool,
    /// Do not print to stdout (the log file still receives entries).
    quiet: bool,
}

/// Escapes a string as a JSON string literal (including the quotes).
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Logger {
    /// Logs a simple event that has no extra structured data.
    fn log(&self, kind: &str, text: &str) {
        self.emit(kind, text, "");
    }

    /// Writes an event to stdout (captured by journald, which already adds a
    /// timestamp) and to the log file (with its own timestamp).
    ///
    /// `fields` holds extra pre-rendered JSON members (without the braces),
    /// e.g. `"device":"sda","util":94`; they are only used in JSON mode.
    fn emit(&self, kind: &str, text: &str, fields: &str) {
        let ts = timestamp();
        let (stdout_line, file_line) = if self.json {
            let extra = if fields.is_empty() {
                String::new()
            } else {
                format!(",{fields}")
            };
            let line = format!(
                "{{\"time\":{},\"event\":{},\"message\":{}{extra}}}",
                json_str(&ts),
                json_str(kind),
                json_str(text)
            );
            (line.clone(), line)
        } else {
            (text.to_string(), format!("{ts} {text}"))
        };

        if !self.quiet {
            println!("{stdout_line}");
        }
        let Some(path) = &self.file else { return };

        // Rotation: past the size limit, the current file becomes `<file>.1`.
        if self.max_bytes > 0
            && fs::metadata(path)
                .map(|m| m.len() > self.max_bytes)
                .unwrap_or(false)
        {
            let mut old = path.clone().into_os_string();
            old.push(".1");
            let _ = fs::rename(path, old);
        }

        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(mut f) => {
                let _ = writeln!(f, "{file_line}");
            }
            Err(e) => eprintln!("warning: could not write to {}: {e}", path.display()),
        }
    }
}

/// libc `struct tm` (64-bit Linux layout).
#[repr(C)]
struct Tm {
    sec: i32,
    min: i32,
    hour: i32,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    isdst: i32,
    gmtoff: i64,
    zone: *const i8,
}

extern "C" {
    fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
}

/// Local date/time formatted as `YYYY-MM-DD HH:MM:SS`.
/// Uses libc's `localtime_r` (already linked by std) to honor the system time
/// zone without needing external libraries.
fn timestamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut tm = Tm {
        sec: 0,
        min: 0,
        hour: 0,
        mday: 0,
        mon: 0,
        year: 0,
        wday: 0,
        yday: 0,
        isdst: 0,
        gmtoff: 0,
        zone: std::ptr::null(),
    };
    // SAFETY: `tm` is valid and writable for the duration of the call; `secs` is only read.
    let ok = unsafe { !localtime_r(&secs, &mut tm).is_null() };
    if !ok {
        return secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.year + 1900,
        tm.mon + 1,
        tm.mday,
        tm.hour,
        tm.min,
        tm.sec
    )
}

// ---------------------------------------------------------------------------
// Automatic disk detection
// ---------------------------------------------------------------------------

/// Finds in `/proc/self/mountinfo` the source (e.g. `/dev/mapper/vg-root`) of
/// the filesystem mounted on `/`. If there are several mounts on `/`, the
/// last one wins (the one on top).
fn parse_root_source(mountinfo: &str) -> Option<String> {
    mountinfo.lines().rev().find_map(|line| {
        // Format: "... mount_point options... - fstype source options"
        let (head, tail) = line.split_once(" - ")?;
        if head.split_whitespace().nth(4)? != "/" {
            return None;
        }
        let source = tail.split_whitespace().nth(1)?;
        source.starts_with("/dev/").then(|| source.to_string())
    })
}

/// Given a block device name (partition, LVM, LUKS...), returns the physical
/// disk underneath it using the sysfs hierarchy:
/// * stacked devices (dm, md) list their components in `slaves/`;
/// * partitions have a `partition` file and live inside the disk's directory.
fn physical_disk(name: &str, depth: u32) -> Option<String> {
    let sys = PathBuf::from("/sys/class/block").join(name);
    if depth > 8 || !sys.exists() {
        return None;
    }
    let first_slave = fs::read_dir(sys.join("slaves")).ok().and_then(|d| {
        d.flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .min()
    });
    if let Some(slave) = first_slave {
        return physical_disk(&slave, depth + 1);
    }
    if sys.join("partition").exists() {
        let real = fs::canonicalize(&sys).ok()?;
        return real.parent()?.file_name()?.to_str().map(String::from);
    }
    Some(name.to_string())
}

/// Detects the disk to monitor: first the one holding `/`; if that is not
/// possible (e.g. overlayfs), the first disk backed by real hardware.
fn detect_device() -> Option<String> {
    let from_root = fs::read_to_string("/proc/self/mountinfo")
        .ok()
        .and_then(|m| parse_root_source(&m))
        .and_then(|src| fs::canonicalize(src).ok()) // /dev/mapper/x -> /dev/dm-0
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .and_then(|n| physical_disk(&n, 0));
    if from_root.is_some() {
        return from_root;
    }
    // Real disks have a `device/` entry; loop, ram and zram are virtual and do not.
    let mut disks: Vec<String> = fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .filter(|e| e.path().join("device").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    disks.sort();
    disks.into_iter().next()
}

// ---------------------------------------------------------------------------
// Disk IO utilization
// ---------------------------------------------------------------------------

/// Extracts the `io_ticks` field (ms during which the disk had IO in flight)
/// of the given device from `/proc/diskstats`.
///
/// Each line is `major minor name field1 field2 ...`, where `io_ticks` is the
/// 10th statistics field, i.e. index 12 of the line.
fn parse_io_ticks(diskstats: &str, device: &str) -> Option<u64> {
    diskstats.lines().find_map(|line| {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.get(2) == Some(&device) {
            parts.get(12)?.parse().ok()
        } else {
            None
        }
    })
}

fn read_io_ticks(device: &str) -> Option<u64> {
    parse_io_ticks(&fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// Utilization percentage (equivalent to `%util` in `iostat -x`):
/// the fraction of elapsed time during which the disk was busy.
fn utilization(delta_ticks_ms: u64, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    (delta_ticks_ms as f64 / elapsed_ms as f64 * 100.0).min(100.0)
}

// ---------------------------------------------------------------------------
// Per-process IO
// ---------------------------------------------------------------------------

/// Bytes actually read from / written to storage by a process
/// (`read_bytes` and `write_bytes` from `/proc/<pid>/io`; page cache is ignored).
#[derive(Clone, Copy, Default, Debug, PartialEq)]
struct ProcIo {
    read: u64,
    write: u64,
}

/// Reads the IO counters of a PID. Returns `None` if the process exited or
/// we lack permission.
fn read_proc_io(pid: u32) -> Option<ProcIo> {
    parse_proc_io(&fs::read_to_string(format!("/proc/{pid}/io")).ok()?)
}

fn parse_proc_io(text: &str) -> Option<ProcIo> {
    let mut io = ProcIo::default();
    let mut found = false;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("read_bytes:") {
            io.read = v.trim().parse().ok()?;
            found = true;
        } else if let Some(v) = line.strip_prefix("write_bytes:") {
            io.write = v.trim().parse().ok()?;
        }
    }
    found.then_some(io)
}

/// Takes a "snapshot" of the IO counters of every visible process.
fn snapshot_processes() -> HashMap<u32, ProcIo> {
    let mut map = HashMap::new();
    let Ok(dir) = fs::read_dir("/proc") else {
        return map;
    };
    for entry in dir.flatten() {
        // Numeric directories under /proc are PIDs.
        if let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        {
            if let Some(io) = read_proc_io(pid) {
                map.insert(pid, io);
            }
        }
    }
    map
}

/// IO done by a process during the measurement window.
struct ProcUsage {
    pid: u32,
    read: u64,
    write: u64,
}

/// Compares two snapshots and returns the `n` processes that did the most IO,
/// in descending order (reads + writes).
fn top_processes(
    before: &HashMap<u32, ProcIo>,
    after: &HashMap<u32, ProcIo>,
    n: usize,
) -> Vec<ProcUsage> {
    let mut usage: Vec<ProcUsage> = after
        .iter()
        .map(|(&pid, now)| {
            // Process started during the window: counts from zero.
            let old = before.get(&pid).copied().unwrap_or_default();
            ProcUsage {
                pid,
                read: now.read.saturating_sub(old.read),
                write: now.write.saturating_sub(old.write),
            }
        })
        .filter(|u| u.read + u.write > 0)
        .collect();
    usage.sort_by_key(|u| std::cmp::Reverse(u.read + u.write));
    usage.truncate(n);
    usage
}

/// Short process name (`/proc/<pid>/comm`).
fn process_name(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "?".into())
}

/// Full command line, truncated so it does not clutter the log.
fn process_cmdline(pid: u32) -> String {
    let raw = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let text: String = String::from_utf8_lossy(&raw)
        .replace('\0', " ")
        .trim()
        .to_string();
    if text.chars().count() > 100 {
        let cut: String = text.chars().take(100).collect();
        format!("{cut}...")
    } else {
        text
    }
}

/// Process owner: the UID from `/proc/<pid>/status` translated via `/etc/passwd`.
fn process_user(pid: u32, users: &HashMap<u32, String>) -> String {
    let uid = fs::read_to_string(format!("/proc/{pid}/status"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("Uid:"))
                .and_then(|v| {
                    v.split_whitespace()
                        .next()
                        .and_then(|u| u.parse::<u32>().ok())
                })
        });
    match uid {
        Some(u) => users.get(&u).cloned().unwrap_or_else(|| u.to_string()),
        None => "?".into(),
    }
}

/// UID -> name map built from `/etc/passwd`.
fn load_users() -> HashMap<u32, String> {
    fs::read_to_string("/etc/passwd")
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split(':');
            let name = f.next()?;
            let uid = f.nth(1)?.parse().ok()?;
            Some((uid, name.to_string()))
        })
        .collect()
}

/// Formats bytes/s in a human-readable way.
fn human_rate(bytes_per_s: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes_per_s >= MB {
        format!("{:.1}MB/s", bytes_per_s / MB)
    } else {
        format!("{:.0}KB/s", bytes_per_s / 1024.0)
    }
}

/// Measures per-process IO over the configured window and writes the report to the log.
fn report_top_processes(cfg: &Config, logger: &Logger, util: f64) {
    let window = cfg.process_window;
    let before = snapshot_processes();
    let start = Instant::now();
    nap(window);
    let after = snapshot_processes();
    // Use the real elapsed time so the rates are accurate.
    let secs = start.elapsed().as_secs_f64().max(0.001);

    let top = top_processes(&before, &after, cfg.top_n);
    let dev = json_str(&cfg.device);
    if top.is_empty() {
        logger.emit(
            "alert",
            &format!(
                "ALERT IO={util:.0}% on {} | no visible process did IO in the window (kernel/journal IO?)",
                cfg.device
            ),
            &format!("\"device\":{dev},\"util\":{util:.0},\"processes\":[]"),
        );
        return;
    }

    let users = load_users();
    let mut list = Vec::new();
    let mut json_list = Vec::new();
    for (i, u) in top.iter().enumerate() {
        let (name, user, cmd) = (
            process_name(u.pid),
            process_user(u.pid, &users),
            process_cmdline(u.pid),
        );
        let (read, write) = (u.read as f64 / secs, u.write as f64 / secs);
        list.push(format!(
            "{}) {} pid={} user={} R={} W={} cmd=\"{}\"",
            i + 1,
            name,
            u.pid,
            user,
            human_rate(read),
            human_rate(write),
            cmd
        ));
        json_list.push(format!(
            "{{\"pid\":{},\"name\":{},\"user\":{},\"read_bytes_per_s\":{:.0},\"write_bytes_per_s\":{:.0},\"cmd\":{}}}",
            u.pid,
            json_str(&name),
            json_str(&user),
            read,
            write,
            json_str(&cmd)
        ));
    }
    logger.emit(
        "alert",
        &format!(
            "ALERT IO={util:.0}% on {} | {}",
            cfg.device,
            list.join(" | ")
        ),
        &format!(
            "\"device\":{dev},\"util\":{util:.0},\"processes\":[{}]",
            json_list.join(",")
        ),
    );
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

/// Configuration file read when `-c` is not given (if it exists).
const DEFAULT_CONFIG: &str = "/etc/iowatchd.conf";

const USAGE: &str = "\
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

/// Options given on the command line; `None` means \"not provided\".
#[derive(Debug, Default, PartialEq)]
struct Cli {
    config: Option<String>,
    device: Option<String>,
    threshold: Option<f64>,
    interval: Option<u64>,
    top_n: Option<usize>,
    json: bool,
    once: bool,
    check_config: bool,
    quiet: bool,
    verbose: bool,
}

/// Result of parsing the arguments.
#[derive(Debug, PartialEq)]
enum Parsed {
    Help,
    Version,
    Run(Cli),
}

impl Cli {
    /// Parses the arguments (without the program name). Accepts
    /// `--option value`, `--option=value` and `-o value`. Errors are
    /// ready-to-display messages (exit code 2).
    fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Parsed, String> {
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
fn build_config(cli: &Cli) -> Result<Config, String> {
    let (cfg, warnings) = load_config(cli)?;
    for w in warnings {
        eprintln!("iowatchd: configuration warning: {w}");
    }
    Ok(cfg)
}

/// `--check-config`: validates the configuration and prints the effective
/// values. Returns the process exit code (1 if there are errors or warnings).
fn check_config(cli: &Cli) -> i32 {
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

// ---------------------------------------------------------------------------
// systemd notification
// ---------------------------------------------------------------------------

/// Sends a state message (e.g. `READY=1`) to systemd through `$NOTIFY_SOCKET`.
/// It is a no-op when not started by systemd with `Type=notify`, and any
/// failure is ignored on purpose: notification is best-effort.
fn sd_notify(message: &str) {
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::net::{SocketAddr, UnixDatagram};

    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    let Ok(sock) = UnixDatagram::unbound() else {
        return;
    };
    let bytes = std::os::unix::ffi::OsStrExt::as_bytes(path.as_os_str());
    let addr = match bytes.strip_prefix(b"@") {
        // A leading '@' denotes a Linux abstract socket.
        Some(name) => SocketAddr::from_abstract_name(name),
        None => SocketAddr::from_pathname(&path),
    };
    if let Ok(addr) = addr {
        let _ = sock.send_to_addr(message.as_bytes(), &addr);
    }
}

// ---------------------------------------------------------------------------
// Signals
// ---------------------------------------------------------------------------

/// Set by the signal handler and polled by the main loop.
static STOP: AtomicBool = AtomicBool::new(false);
static RELOAD: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn sleep(seconds: u32) -> u32;
}

/// Signal handler: only touches atomics, which is safe in signal context.
extern "C" fn on_signal(signum: i32) {
    if signum == 1 {
        RELOAD.store(true, Ordering::SeqCst); // SIGHUP
    } else {
        STOP.store(true, Ordering::SeqCst); // SIGINT / SIGTERM
    }
}

fn install_signal_handlers() {
    // SAFETY: `on_signal` only uses atomic operations, which are signal-safe.
    unsafe {
        signal(1, on_signal); // SIGHUP
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

/// Sleeps for `secs` seconds, but wakes up immediately when a signal arrives
/// (`std::thread::sleep` would go back to sleep), so stop and reload take
/// effect without delay.
fn nap(secs: u64) {
    // SAFETY: plain libc call, no pointers involved.
    unsafe { sleep(secs.min(u32::MAX as u64) as u32) };
}

// ---------------------------------------------------------------------------
// Main program
// ---------------------------------------------------------------------------

/// Monitor state: IO normal or in alert.
enum State {
    Normal { consecutive_high: u32 },
    Alert { last_report: Instant },
}

/// `--once` mode: measures for one interval, prints, and lists the processes
/// if above the threshold. Does not write to the log file.
fn run_once(cfg: &Config, cli: &Cli) {
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

fn make_logger(cfg: &Config, cli: &Cli) -> Logger {
    Logger {
        file: cfg.log_file.clone(),
        max_bytes: cfg.log_max_bytes,
        json: cfg.log_json,
        quiet: cli.quiet,
    }
}

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

    let mut cfg = build_config(&cli).unwrap_or_else(|e| {
        eprintln!("iowatchd: error: {e}");
        std::process::exit(1);
    });

    install_signal_handlers();

    if cli.once {
        run_once(&cfg, &cli);
        return;
    }

    let mut logger = make_logger(&cfg, &cli);
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
            match build_config(&cli) {
                Ok(new) => {
                    cfg = new;
                    logger = make_logger(&cfg, &cli);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_ticks_is_read_from_the_right_device() {
        let s = "   8       0 sda 100 0 800 50 200 0 1600 70 0 4242 120 0 0 0 0 0 0\n\
                 \x20  8       1 sda1 1 0 8 5 2 0 16 7 0 99 12 0 0 0 0 0 0\n";
        assert_eq!(parse_io_ticks(s, "sda"), Some(4242));
        assert_eq!(parse_io_ticks(s, "sda1"), Some(99));
        assert_eq!(parse_io_ticks(s, "nvme0n1"), None);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn config_reads_log_format() {
        let (c, w) = Config::parse("log_format = json\nlog_max_mb = 0\n");
        assert!(c.log_json && w.is_empty());
        assert_eq!(c.log_max_bytes, 0);
        let (c, w) = Config::parse("log_format = xml\n");
        assert!(!c.log_json);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn cli_accepts_new_flags() {
        let a = |v: &[&str]| Cli::parse(v.iter().map(|s| s.to_string()));
        let Ok(Parsed::Run(c)) = a(&["-j", "-q", "-v", "-C", "-f"]) else {
            panic!("parse failed")
        };
        assert!(c.json && c.quiet && c.verbose && c.check_config);
    }

    #[test]
    fn cli_accepts_long_short_and_equals_forms() {
        let a = |v: &[&str]| Cli::parse(v.iter().map(|s| s.to_string()));
        assert_eq!(a(&["-h"]), Ok(Parsed::Help));
        assert_eq!(a(&["--version"]), Ok(Parsed::Version));
        let Ok(Parsed::Run(c)) = a(&["-d", "nvme0n1", "--threshold=60", "-i", "5", "--once"])
        else {
            panic!("parse failed")
        };
        assert_eq!(c.device.as_deref(), Some("nvme0n1"));
        assert_eq!(c.threshold, Some(60.0));
        assert_eq!(c.interval, Some(5));
        assert!(c.once);
    }

    #[test]
    fn cli_rejects_invalid_input() {
        let a = |v: &[&str]| Cli::parse(v.iter().map(|s| s.to_string()));
        assert!(a(&["--bogus"]).is_err());
        assert!(a(&["file"]).is_err());
        assert!(a(&["-t"]).is_err());
        assert!(a(&["-t", "0"]).is_err());
        assert!(a(&["-t", "101"]).is_err());
        assert!(a(&["-i", "0"]).is_err());
        assert!(a(&["-n", "abc"]).is_err());
    }

    #[test]
    fn root_source_is_extracted_from_mountinfo() {
        let m = "22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
                 30 22 0:25 / /tmp rw - tmpfs tmpfs rw\n\
                 40 22 0:40 /@ / rw - btrfs /dev/mapper/cryptroot rw\n";
        assert_eq!(
            parse_root_source(m).as_deref(),
            Some("/dev/mapper/cryptroot")
        );
        assert_eq!(
            parse_root_source("1 0 0:1 / / rw - overlay overlay rw\n"),
            None
        );
    }

    #[test]
    fn utilization_is_capped_at_100() {
        assert_eq!(utilization(500, 1000), 50.0);
        assert_eq!(utilization(2000, 1000), 100.0);
        assert_eq!(utilization(10, 0), 0.0);
    }

    #[test]
    fn proc_io_reads_real_bytes() {
        let t = "rchar: 10\nwchar: 20\nread_bytes: 4096\nwrite_bytes: 8192\n";
        assert_eq!(
            parse_proc_io(t),
            Some(ProcIo {
                read: 4096,
                write: 8192
            })
        );
        assert_eq!(parse_proc_io("rchar: 1\n"), None);
    }

    #[test]
    fn top_sorts_and_ignores_processes_without_io() {
        let before: HashMap<u32, ProcIo> = [(1, ProcIo { read: 0, write: 0 })].into();
        let after: HashMap<u32, ProcIo> = [
            (1, ProcIo { read: 0, write: 0 }),
            (
                2,
                ProcIo {
                    read: 100,
                    write: 0,
                },
            ),
            (
                3,
                ProcIo {
                    read: 0,
                    write: 900,
                },
            ),
        ]
        .into();
        let top = top_processes(&before, &after, 5);
        assert_eq!(top.iter().map(|u| u.pid).collect::<Vec<_>>(), vec![3, 2]);
        assert_eq!(top_processes(&before, &after, 1).len(), 1);
    }

    #[test]
    fn config_applies_values_and_warns_about_errors() {
        let (c, w) = Config::parse(
            "device = nvme0n1\nthreshold=90 # x\ninterval = abc\nfoo = 1\nlog_file =\n",
        );
        assert_eq!(c.device, "nvme0n1");
        assert_eq!(c.threshold, 90.0);
        assert_eq!(c.interval, 2);
        assert!(c.log_file.is_none());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn config_fixes_inverted_hysteresis() {
        let (c, _) = Config::parse("threshold = 50\nclear_threshold = 70\ninterval = 0\n");
        assert_eq!(c.clear_threshold, 50.0);
        assert_eq!(c.interval, 1);
    }
}
