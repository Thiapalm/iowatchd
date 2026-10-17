//! `--report`: summarizes the alerts found in the log file, i.e. which
//! processes and services caused the most IO alerts in a period.
//!
//! Works with both log formats (text and JSON lines) and also reads the
//! rotated `<file>.1`.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
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
    hits: Vec<Hit>,
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
    let hits = body.split(" | ").skip(1).filter_map(parse_hit).collect();
    Some(Alert { time, util, hits })
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
}

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

fn render_text(s: &Summary, since_label: &str, log: &Path, top: usize) -> String {
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

fn render_json(s: &Summary, since_secs: u64, top: usize) -> String {
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
    format!(
        "{{\"since_seconds\":{since_secs},\"alerts\":{},\"peak_util\":{:.0},\"first\":{},\"last\":{},\"offenders\":[{}]}}",
        s.alerts,
        s.peak_util,
        json_str(&s.first),
        json_str(&s.last),
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
pub fn run(log_file: Option<&Path>, since_secs: u64, top: usize, json: bool) -> i32 {
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
    if json {
        println!("{}", render_json(&summary, since_secs, top));
    } else {
        print!("{}", render_text(&summary, &label(since_secs), path, top));
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
        assert!(render_text(&s, "24h", Path::new("/x"), 5).contains("brave"));
        assert!(render_json(&s, 86400, 5).starts_with("{\"since_seconds\":86400,\"alerts\":2"));
    }
}
