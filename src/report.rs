//! `--report`: summarizes the alerts found in the log file, i.e. which
//! processes and services caused the most IO alerts in a period.
//!
//! Works with both log formats (text and JSON lines) and also reads the
//! rotated `<file>.1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::logger::{format_time, json_str};
use crate::procs::human_rate;

/// Default report period when `--since` is not given.
pub const DEFAULT_SINCE_SECS: u64 = 24 * 3600;

/// Parses a period such as `90`, `90s`, `15m`, `24h`, `7d` or `2w` into seconds.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(text.len());
    let (num, unit) = text.split_at(split);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 7 * 86400,
        _ => return None,
    };
    n.checked_mul(mult).filter(|&s| s > 0)
}

/// One process or service listed in an alert.
#[derive(Debug, PartialEq)]
struct Hit {
    name: String,
    unit: String,
    /// Read + write rate in bytes per second.
    rate: f64,
}

/// One `ALERT` line of the log.
#[derive(Debug, PartialEq)]
struct Alert {
    /// Local time as `YYYY-MM-DD HH:MM:SS`.
    time: String,
    util: f64,
    device: String,
    /// Metrics from the alert header; `None` when the line lacks them.
    await_ms: Option<f64>,
    /// Disk read + write throughput in bytes per second.
    throughput: Option<f64>,
    psi: Option<f64>,
    hits: Vec<Hit>,
}

/// Value of `key=` in the alert header, up to the next space or `)`.
fn header_field<'a>(header: &'a str, key: &str) -> Option<&'a str> {
    let rest = header.split_once(key)?.1;
    rest.split([' ', ')']).next()
}

fn header_number(header: &str, key: &str) -> Option<f64> {
    header_field(header, key)?
        .trim_end_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()
}

/// Converts `56KB/s` or `1.2MB/s` to bytes per second.
fn parse_rate(text: &str) -> f64 {
    let text = text.trim_end_matches("/s");
    if let Some(v) = text.strip_suffix("MB") {
        v.parse::<f64>().unwrap_or(0.0) * 1024.0 * 1024.0
    } else if let Some(v) = text.strip_suffix("KB") {
        v.parse::<f64>().unwrap_or(0.0) * 1024.0
    } else {
        0.0
    }
}

/// Parses one process entry: `1) name pid=1 user=x unit=y R=0KB/s W=3KB/s cmd="..."`.
fn parse_hit(segment: &str) -> Option<Hit> {
    let (_, rest) = segment.split_once(") ")?;
    let (name, rest) = rest.split_once(" pid=")?;
    // The command line may contain anything, so stop before it.
    let fields = rest.split(" cmd=").next()?;
    let mut unit = String::new();
    let mut rate = 0.0;
    for token in fields.split_whitespace() {
        if let Some(u) = token.strip_prefix("unit=") {
            unit = u.to_string();
        } else if let Some(r) = token
            .strip_prefix("R=")
            .or_else(|| token.strip_prefix("W="))
        {
            rate += parse_rate(r);
        }
    }
    Some(Hit {
        name: name.to_string(),
        unit,
        rate,
    })
}

/// Parses a log line (either format); `None` if it is not an alert.
fn parse_alert(line: &str) -> Option<Alert> {
    let start = line.find("ALERT IO=")?;
    let time = if line.starts_with('{') {
        let t = line.split_once("\"time\":\"")?.1;
        t.get(..19)?.to_string()
    } else {
        line.get(..19)?.to_string()
    };
    let body = &line[start + "ALERT IO=".len()..];
    let util = body
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()?
        .parse()
        .ok()?;
    let mut parts = body.split(" | ");
    let header = parts.next().unwrap_or("");
    let device = header
        .split_once(" on ")
        .and_then(|(_, r)| r.split_whitespace().next())
        .unwrap_or("")
        .to_string();
    let throughput = match (
        header_field(header, "read="),
        header_field(header, "write="),
    ) {
        (Some(r), Some(w)) => Some(parse_rate(r) + parse_rate(w)),
        _ => None,
    };
    let hits = parts.filter_map(parse_hit).collect();
    Some(Alert {
        time,
        util,
        device,
        await_ms: header_number(header, "await="),
        throughput,
        psi: header_number(header, "psi="),
        hits,
    })
}

/// Aggregated data of one process/service.
#[derive(Default)]
struct Offender {
    alerts: u32,
    total_rate: f64,
    peak_rate: f64,
}

/// The result of analyzing the log.
#[derive(Default)]
struct Summary {
    alerts: u32,
    peak_util: f64,
    first: String,
    last: String,
    by_hour: BTreeMap<String, u32>,
    offenders: BTreeMap<(String, String), Offender>,
    device: String,
    /// Alerts with high latency but almost no traffic (a sick disk, not a busy one).
    stalls: u32,
    /// Alerts with high throughput and low latency (the disk is fast when it works).
    healthy_peaks: u32,
    /// Alerts where no process was seen doing IO.
    unattributed: u32,
    peak_await: f64,
    peak_psi: f64,
    /// Sum of await and number of alerts that have it, for the average.
    await_sum: f64,
    await_n: u32,
}

/// Latency (ms) above which a request is considered a stall.
const STALL_AWAIT_MS: f64 = 200.0;
/// Throughput (bytes/s) below which a stall is not explained by traffic.
const STALL_MAX_RATE: f64 = 1024.0 * 1024.0;
/// A healthy burst: at least this throughput with latency under `HEALTHY_AWAIT_MS`.
const HEALTHY_MIN_RATE: f64 = 10.0 * 1024.0 * 1024.0;
const HEALTHY_AWAIT_MS: f64 = 20.0;

/// Aggregates the alerts whose time is not before `cutoff`
/// (same `YYYY-MM-DD HH:MM:SS` format, so strings compare chronologically).
fn analyze<'a>(lines: impl Iterator<Item = &'a str>, cutoff: &str) -> Summary {
    let mut s = Summary::default();
    for alert in lines.filter_map(parse_alert) {
        if alert.time.as_str() < cutoff {
            continue;
        }
        s.alerts += 1;
        s.peak_util = s.peak_util.max(alert.util);
        if s.first.is_empty() || alert.time < s.first {
            s.first = alert.time.clone();
        }
        if alert.time > s.last {
            s.last = alert.time.clone();
        }
        if let Some(h) = alert.time.get(11..13) {
            *s.by_hour.entry(h.to_string()).or_default() += 1;
        }
        if !alert.device.is_empty() {
            s.device = alert.device.clone();
        }
        if let Some(a) = alert.await_ms {
            s.peak_await = s.peak_await.max(a);
            s.await_sum += a;
            s.await_n += 1;
            match alert.throughput {
                Some(t) if a >= STALL_AWAIT_MS && t < STALL_MAX_RATE => s.stalls += 1,
                Some(t) if a < HEALTHY_AWAIT_MS && t >= HEALTHY_MIN_RATE => s.healthy_peaks += 1,
                _ => {}
            }
        }
        s.peak_psi = s.peak_psi.max(alert.psi.unwrap_or(0.0));
        if alert.hits.is_empty() {
            s.unattributed += 1;
        }
        for hit in alert.hits {
            let o = s.offenders.entry((hit.name, hit.unit)).or_default();
            o.alerts += 1;
            o.total_rate += hit.rate;
            o.peak_rate = o.peak_rate.max(hit.rate);
        }
    }
    s
}

/// Offenders sorted by number of alerts, then by average rate.
fn ranked(s: &Summary, top: usize) -> Vec<(&(String, String), &Offender)> {
    let mut v: Vec<_> = s.offenders.iter().collect();
    v.sort_by(|a, b| {
        b.1.alerts.cmp(&a.1.alerts).then(
            (b.1.total_rate / b.1.alerts as f64).total_cmp(&(a.1.total_rate / a.1.alerts as f64)),
        )
    });
    v.truncate(top);
    v
}

/// Findings drawn from the alert metrics (no extra privileges needed).
fn diagnose(s: &Summary) -> Vec<String> {
    let mut out = Vec::new();
    if s.await_n == 0 {
        return out;
    }
    let stall_share = s.stalls as f64 / s.alerts as f64;
    if s.stalls >= 3 && stall_share >= 0.5 {
        out.push(format!(
            "{} of {} alerts ({:.0}%) show high latency (await >= {STALL_AWAIT_MS:.0} ms) with \
             almost no traffic (< 1 MB/s): the disk is slow, not busy. This points to a \
             degrading disk/SSD (bad cells, retries) rather than to any process.",
            s.stalls,
            s.alerts,
            stall_share * 100.0
        ));
        out.push(format!(
            "await averaged {:.0} ms (peak {:.0} ms); IO pressure (psi) peaked at {:.0}%, \
             so tasks were really blocked.",
            s.await_sum / s.await_n as f64,
            s.peak_await,
            s.peak_psi
        ));
        if s.healthy_peaks > 0 {
            out.push(format!(
                "{} alert(s) had high throughput with low latency: the disk is fast when it \
                 works, so the problem is intermittent stalls.",
                s.healthy_peaks
            ));
        }
        out.push(
            "Processes listed with only a few KB/s are victims waiting for the disk, \
             not the cause."
                .into(),
        );
    } else if s.peak_await >= STALL_AWAIT_MS {
        out.push(format!(
            "Latency reached {:.0} ms, but most alerts coincide with real traffic: \
             check the top offenders below.",
            s.peak_await
        ));
    }
    if s.unattributed > 0 && s.unattributed * 4 >= s.alerts {
        out.push(format!(
            "{} alert(s) had no process doing IO (kernel, journal or the disk itself).",
            s.unattributed
        ));
    }
    out
}

/// A SMART attribute that signals trouble when its raw value is above zero.
#[derive(Debug, PartialEq)]
struct SmartFinding {
    id: u32,
    name: &'static str,
    raw: u64,
}

/// Standard attributes whose consistent meaning makes them safe to judge
/// across vendors (many SSDs expose the rest as "Unknown_Attribute").
const SMART_WATCH: [(u32, &str); 4] = [
    (5, "Reallocated_Sector_Ct"),
    (196, "Reallocated_Event_Count"),
    (197, "Current_Pending_Sector"),
    (198, "Offline_Uncorrectable"),
];

/// Extracts the watched attributes with a non-zero raw value from `smartctl -A`.
fn parse_smart(output: &str) -> Vec<SmartFinding> {
    let mut found = Vec::new();
    for line in output.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        let Some(id) = cols.first().and_then(|c| c.parse::<u32>().ok()) else {
            continue;
        };
        let Some(&(_, name)) = SMART_WATCH.iter().find(|(i, _)| *i == id) else {
            continue;
        };
        // The raw value is the 10th column; it may be followed by extra text.
        let raw = cols.get(9).and_then(|r| r.parse::<u64>().ok()).unwrap_or(0);
        if raw > 0 {
            found.push(SmartFinding { id, name, raw });
        }
    }
    found
}

/// SMART result: findings, or why it could not be read.
enum Smart {
    Findings(Vec<SmartFinding>),
    Unavailable(String),
}

fn read_smart(device: &str) -> Smart {
    if device.is_empty() || !device.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Smart::Unavailable("device unknown".into());
    }
    let out = match Command::new("smartctl")
        .args(["-A", &format!("/dev/{device}")])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Smart::Unavailable("smartctl not installed (smartmontools)".into()),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if !text.contains("ATTRIBUTE_NAME") {
        return Smart::Unavailable(format!(
            "could not read SMART data; try: sudo iowatchd --report (or smartctl -A /dev/{device})"
        ));
    }
    Smart::Findings(parse_smart(&text))
}

fn smart_lines(smart: &Smart) -> Vec<String> {
    match smart {
        Smart::Unavailable(why) => vec![format!("unavailable ({why})")],
        Smart::Findings(f) if f.is_empty() => {
            vec!["OK: no reallocated, pending or uncorrectable sectors.".into()]
        }
        Smart::Findings(f) => {
            let mut v = vec!["WARNING: the disk reports sector problems:".to_string()];
            for x in f {
                v.push(format!("    {} {} = {}", x.id, x.name, x.raw));
            }
            v.push(
                "Back up your data now and consider replacing the disk. Check: \
                 smartctl -a, dmesg, and a long self-test (smartctl -t long)."
                    .into(),
            );
            v
        }
    }
}

fn render_text(
    s: &Summary,
    since_label: &str,
    log: &Path,
    top: usize,
    smart: Option<&Smart>,
) -> String {
    let mut out = format!(
        "iowatchd report: alerts in the last {since_label} (from {})\n",
        log.display()
    );
    if s.alerts == 0 {
        out.push_str("No IO alerts in this period.\n");
        return out;
    }
    out.push_str(&format!(
        "alerts: {}   peak IO: {:.0}%   first: {}   last: {}\n",
        s.alerts, s.peak_util, s.first, s.last
    ));
    out.push_str("\nAlerts by hour of day:\n");
    let max = s.by_hour.values().copied().max().unwrap_or(1).max(1);
    for (hour, n) in &s.by_hour {
        let bar = "#".repeat((n * 30 / max).max(1) as usize);
        out.push_str(&format!("  {hour}h  {bar} {n}\n"));
    }
    let notes = diagnose(s);
    if !notes.is_empty() {
        out.push_str("\nDiagnosis:\n");
        for n in notes {
            out.push_str(&format!("  - {n}\n"));
        }
    }
    if let Some(sm) = smart {
        out.push_str(&format!("\n=== SMART ({}) ===\n", s.device));
        for l in smart_lines(sm) {
            out.push_str(&format!("  {l}\n"));
        }
    }
    if s.offenders.is_empty() {
        out.push_str("\nNo process was identified in these alerts.\n");
        return out;
    }
    out.push_str("\nTop offenders (by number of alerts they appeared in):\n");
    out.push_str(&format!(
        "  {:>2}  {:<20} {:<24} {:>6}  {:>9}  {:>9}\n",
        "#", "process", "unit", "alerts", "avg IO", "peak IO"
    ));
    for (i, ((name, unit), o)) in ranked(s, top).into_iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}  {:<20} {:<24} {:>6}  {:>9}  {:>9}\n",
            i + 1,
            name,
            if unit.is_empty() { "-" } else { unit },
            o.alerts,
            human_rate(o.total_rate / o.alerts as f64),
            human_rate(o.peak_rate)
        ));
    }
    out
}

fn render_json(s: &Summary, since_secs: u64, top: usize, smart: Option<&Smart>) -> String {
    let offenders: Vec<String> = ranked(s, top)
        .into_iter()
        .map(|((name, unit), o)| {
            format!(
                "{{\"name\":{},\"unit\":{},\"alerts\":{},\"avg_bytes_per_s\":{:.0},\"peak_bytes_per_s\":{:.0}}}",
                json_str(name),
                json_str(unit),
                o.alerts,
                o.total_rate / o.alerts as f64,
                o.peak_rate
            )
        })
        .collect();
    let findings: Vec<String> = diagnose(s).iter().map(|f| json_str(f)).collect();
    let smart_json = match smart {
        None => "null".to_string(),
        Some(Smart::Unavailable(why)) => {
            format!("{{\"available\":false,\"reason\":{}}}", json_str(why))
        }
        Some(Smart::Findings(f)) => {
            let items: Vec<String> = f
                .iter()
                .map(|x| {
                    format!(
                        "{{\"id\":{},\"name\":{},\"raw\":{}}}",
                        x.id,
                        json_str(x.name),
                        x.raw
                    )
                })
                .collect();
            format!("{{\"available\":true,\"problems\":[{}]}}", items.join(","))
        }
    };
    format!(
        "{{\"since_seconds\":{since_secs},\"alerts\":{},\"peak_util\":{:.0},\"first\":{},\"last\":{},\"stalls\":{},\"peak_await_ms\":{:.1},\"diagnosis\":[{}],\"smart\":{},\"offenders\":[{}]}}",
        s.alerts,
        s.peak_util,
        json_str(&s.first),
        json_str(&s.last),
        s.stalls,
        s.peak_await,
        findings.join(","),
        smart_json,
        offenders.join(",")
    )
}

/// Human label for a period in seconds (`24h`, `7d`, ...).
fn label(secs: u64) -> String {
    match secs {
        s if s % 86400 == 0 => format!("{}d", s / 86400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Runs the report and returns the process exit code.
pub fn run(
    log_file: Option<&Path>,
    since_secs: u64,
    top: usize,
    json: bool,
    use_smart: bool,
) -> i32 {
    let Some(path) = log_file else {
        eprintln!(
            "iowatchd: error: no log_file configured; the report needs the log file \
             (journal-only setups: use 'iowatchd-log')"
        );
        return 1;
    };
    // The rotated file first, so lines stay in chronological order.
    let mut old = path.as_os_str().to_os_string();
    old.push(".1");
    let mut text = fs::read_to_string(&old).unwrap_or_default();
    match fs::read_to_string(path) {
        Ok(t) => text.push_str(&t),
        Err(e) if text.is_empty() => {
            eprintln!("iowatchd: error: could not read {}: {e}", path.display());
            return 1;
        }
        Err(_) => {}
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let cutoff = format_time(now - since_secs as i64);
    let summary = analyze(text.lines(), &cutoff);
    let smart = (use_smart && summary.alerts > 0).then(|| read_smart(&summary.device));
    if json {
        println!("{}", render_json(&summary, since_secs, top, smart.as_ref()));
    } else {
        print!(
            "{}",
            render_text(&summary, &label(since_secs), path, top, smart.as_ref())
        );
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT: &str = "2026-10-17 10:00:02 ALERT IO=97% on sda (await=45.2ms queue=3.1 read=0KB/s write=60KB/s psi=12.0%) | 1) plasmashell pid=987 user=thiago unit=app-plasmashell R=0KB/s W=48KB/s cmd=\"/usr/bin/plasmashell --no-respawn\" | 2) brave pid=2163 user=thiago R=1.0MB/s W=12KB/s cmd=\"brave | x\"";

    #[test]
    fn durations_are_parsed() {
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("15m"), Some(900));
        assert_eq!(parse_duration("24h"), Some(86400));
        assert_eq!(parse_duration("7d"), Some(604800));
        assert_eq!(parse_duration("0"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("5x"), None);
    }

    #[test]
    fn text_alert_is_parsed() {
        let a = parse_alert(TEXT).unwrap();
        assert_eq!(a.time, "2026-10-17 10:00:02");
        assert_eq!(a.util, 97.0);
        assert_eq!(a.hits.len(), 2);
        assert_eq!(a.hits[0].name, "plasmashell");
        assert_eq!(a.hits[0].unit, "app-plasmashell");
        assert_eq!(a.hits[0].rate, 48.0 * 1024.0);
        assert_eq!(a.hits[1].unit, "");
        assert_eq!(a.hits[1].rate, 1024.0 * 1024.0 + 12.0 * 1024.0);
    }

    #[test]
    fn json_alert_is_parsed() {
        let line = "{\"time\":\"2026-10-17 11:30:00\",\"event\":\"alert\",\"message\":\"ALERT IO=100% on sda | 1) cron pid=5 user=root unit=cron R=0KB/s W=8KB/s cmd=\\\"cron\\\"\",\"device\":\"sda\"}";
        let a = parse_alert(line).unwrap();
        assert_eq!(a.time, "2026-10-17 11:30:00");
        assert_eq!(a.util, 100.0);
        assert_eq!(a.hits[0].name, "cron");
        assert_eq!(a.hits[0].rate, 8.0 * 1024.0);
        assert!(parse_alert("2026-10-17 10:00:00 IO back to normal: 10% on sda").is_none());
    }

    #[test]
    fn summary_filters_by_time_and_ranks() {
        let old = TEXT.replace("2026-10-17 10:00:02", "2026-10-10 10:00:02");
        let later = TEXT.replace("10:00:02", "11:00:00");
        let log = format!("{old}\n{TEXT}\n{later}\n");
        let s = analyze(log.lines(), "2026-10-17 00:00:00");
        assert_eq!(s.alerts, 2);
        assert_eq!(s.peak_util, 97.0);
        assert_eq!(s.last, "2026-10-17 11:00:00");
        assert_eq!(s.by_hour.get("10"), Some(&1));
        let top = ranked(&s, 1);
        assert_eq!(top[0].1.alerts, 2);
        assert_eq!(top[0].0 .0, "brave"); // higher average rate wins the tie
        assert!(render_text(&s, "24h", Path::new("/x"), 5, None).contains("brave"));
        assert!(
            render_json(&s, 86400, 5, None).starts_with("{\"since_seconds\":86400,\"alerts\":2")
        );
    }

    #[test]
    fn header_metrics_are_parsed() {
        let a = parse_alert(TEXT).unwrap();
        assert_eq!(a.device, "sda");
        assert_eq!(a.await_ms, Some(45.2));
        assert_eq!(a.psi, Some(12.0));
        assert_eq!(a.throughput, Some(60.0 * 1024.0));
    }

    #[test]
    fn slow_disk_is_diagnosed() {
        let line = "2026-10-17 20:27:29 ALERT IO=100% on sda (await=624.9ms queue=3.0 read=256KB/s write=22KB/s psi=76.3%) | no visible process did IO";
        let log = format!("{line}\n{line}\n{line}\n");
        let s = analyze(log.lines(), "2026-10-17 00:00:00");
        assert_eq!(s.stalls, 3);
        assert_eq!(s.unattributed, 3);
        let text = render_text(&s, "24h", Path::new("/x"), 5, None);
        assert!(text.contains("Diagnosis:") && text.contains("the disk is slow, not busy"));
        assert!(render_json(&s, 86400, 5, None).contains("\"stalls\":3"));
    }

    #[test]
    fn busy_disk_is_not_called_slow() {
        let s = analyze(TEXT.lines(), "2026-10-17 00:00:00");
        assert_eq!(s.stalls, 0);
        assert!(diagnose(&s).is_empty());
    }

    #[test]
    fn smart_attributes_are_parsed() {
        let out = "ID# ATTRIBUTE_NAME FLAG VALUE WORST THRESH TYPE UPDATED WHEN_FAILED RAW_VALUE\n  5 Reallocated_Sector_Ct   0x0032   100   100   050    Old_age   Always       -       2\n196 Reallocated_Event_Count 0x0032   100   100   050    Old_age   Always       -       0\n197 Current_Pending_Sector  0x0032   100   100   050    Old_age   Always       -       2\n";
        let f = parse_smart(out);
        assert_eq!(f.len(), 2);
        assert_eq!((f[1].id, f[1].raw), (197, 2));
        assert!(smart_lines(&Smart::Findings(f))[0].contains("sector problems"));
    }

    #[test]
    fn rates_are_converted_to_bytes() {
        assert_eq!(parse_rate("56KB/s"), 56.0 * 1024.0);
        assert_eq!(parse_rate("1.5MB/s"), 1.5 * 1024.0 * 1024.0);
        assert_eq!(parse_rate("garbage"), 0.0);
    }

    #[test]
    fn malformed_lines_are_not_alerts() {
        assert!(parse_hit("no separator here").is_none());
        assert!(parse_alert("random text").is_none());
        assert!(parse_alert("").is_none());
    }
}
