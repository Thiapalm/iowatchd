//! Per-process IO: snapshots of `/proc/<pid>/io`, ranking and the alert report.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use crate::config::Config;
use crate::disk::{read_pressure, Metrics, Pressure};
use crate::logger::{json_str, Logger};
use crate::signals::nap;

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
pub fn human_rate(bytes_per_s: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes_per_s >= MB {
        format!("{:.1}MB/s", bytes_per_s / MB)
    } else {
        format!("{:.0}KB/s", bytes_per_s / 1024.0)
    }
}

/// Extracts the systemd unit / container scope from the contents of
/// `/proc/<pid>/cgroup`, e.g. `app-brave@1234.service` or `docker-1a2b3c4d5e6f`.
/// Returns an empty string when the process is in the root cgroup.
fn parse_cgroup_unit(text: &str) -> String {
    // cgroup v2 has a single `0::<path>` line; v1 lists several hierarchies.
    let path = text
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .or_else(|| {
            text.lines()
                .find(|l| l.contains("name=systemd:"))
                .and_then(|l| l.split_once("name=systemd:").map(|(_, p)| p))
        })
        .unwrap_or("");
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    // The outermost service/scope names the application (inner ones are
    // often anonymous, like terminal tabs); `user@N.service` is only the
    // per-user manager.
    let unit = parts
        .iter()
        .find(|p| (p.ends_with(".service") || p.ends_with(".scope")) && !p.starts_with("user@"))
        .or(parts.last());
    unit.map(|u| shorten_unit(u)).unwrap_or_default()
}

/// Drops the `.service`/`.scope` suffix and abbreviates container IDs
/// (hex strings of 32+ characters) to 12 characters.
fn shorten_unit(unit: &str) -> String {
    let base = unit
        .strip_suffix(".service")
        .or_else(|| unit.strip_suffix(".scope"))
        .unwrap_or(unit);
    base.split('-')
        .map(|piece| {
            if piece.len() >= 32 && piece.bytes().all(|b| b.is_ascii_hexdigit()) {
                &piece[..12]
            } else {
                piece
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

/// The service or container a process belongs to.
fn process_unit(pid: u32) -> String {
    fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map(|t| parse_cgroup_unit(&t))
        .unwrap_or_default()
}

/// The disk-level details shown in the alert header, e.g.
/// ` (await=12.3ms queue=1.5 read=0KB/s write=48KB/s psi=3.2%)`.
fn metrics_text(m: &Metrics, psi: Option<Pressure>) -> String {
    let mut parts = Vec::new();
    if let Some(a) = m.await_ms {
        parts.push(format!("await={a:.1}ms"));
    }
    parts.push(format!("queue={:.1}", m.queue_depth));
    parts.push(format!("read={}", human_rate(m.read_bytes_per_s)));
    parts.push(format!("write={}", human_rate(m.write_bytes_per_s)));
    if let Some(p) = psi {
        parts.push(format!("psi={:.1}%", p.some_avg10));
    }
    format!(" ({})", parts.join(" "))
}

/// The same details as JSON members (without braces).
fn metrics_json(m: &Metrics, psi: Option<Pressure>) -> String {
    let mut out = format!(
        "\"queue_depth\":{:.2},\"disk_read_bytes_per_s\":{:.0},\"disk_write_bytes_per_s\":{:.0}",
        m.queue_depth, m.read_bytes_per_s, m.write_bytes_per_s
    );
    if let Some(a) = m.await_ms {
        out.push_str(&format!(",\"await_ms\":{a:.2}"));
    }
    if let Some(p) = psi {
        out.push_str(&format!(
            ",\"psi_some_avg10\":{:.2},\"psi_full_avg10\":{:.2}",
            p.some_avg10, p.full_avg10
        ));
    }
    out
}

/// One-line disk details for samples (`--once`, `--verbose`).
pub fn sample_text(m: &Metrics) -> String {
    metrics_text(m, read_pressure())
}

/// Per-process IO of other users is only readable by root (or with
/// CAP_SYS_PTRACE). Returns a warning when that is not the case.
pub fn unprivileged_warning() -> Option<&'static str> {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: plain libc call, no arguments.
    let root = unsafe { geteuid() } == 0;
    // Ambient capabilities (the systemd unit) also work: probe PID 1.
    let can_read_pid1 = fs::read_to_string("/proc/1/io").is_ok();
    (!root && !can_read_pid1)
        .then_some("not running as root: only your own processes can be inspected")
}

/// Measures per-process IO over the configured window and writes the report
/// to the log. Returns the logged message (for the notification hook).
pub fn report_top_processes(cfg: &Config, logger: &Logger, m: &Metrics) -> String {
    let util = m.util;
    let window = cfg.process_window;
    let before = snapshot_processes();
    let start = Instant::now();
    nap(window);
    let after = snapshot_processes();
    // Use the real elapsed time so the rates are accurate.
    let secs = start.elapsed().as_secs_f64().max(0.001);
    let psi = read_pressure();
    let head = format!(
        "ALERT IO={util:.0}% on {}{}",
        cfg.device,
        metrics_text(m, psi)
    );
    let base_json = format!(
        "\"device\":{},\"util\":{util:.0},{}",
        json_str(&cfg.device),
        metrics_json(m, psi)
    );

    let top = top_processes(&before, &after, cfg.top_n);
    if top.is_empty() {
        let hint = if unprivileged_warning().is_some() {
            "run as root to see other users' processes"
        } else {
            "kernel/journal IO?"
        };
        let msg = format!("{head} | no visible process did IO in the window ({hint})");
        logger.emit("alert", &msg, &format!("{base_json},\"processes\":[]"));
        return msg;
    }

    let users = load_users();
    let mut list = Vec::new();
    let mut json_list = Vec::new();
    for (i, u) in top.iter().enumerate() {
        let (name, user, unit, cmd) = (
            process_name(u.pid),
            process_user(u.pid, &users),
            process_unit(u.pid),
            process_cmdline(u.pid),
        );
        let (read, write) = (u.read as f64 / secs, u.write as f64 / secs);
        let unit_text = if unit.is_empty() {
            String::new()
        } else {
            format!(" unit={unit}")
        };
        list.push(format!(
            "{}) {} pid={} user={}{} R={} W={} cmd=\"{}\"",
            i + 1,
            name,
            u.pid,
            user,
            unit_text,
            human_rate(read),
            human_rate(write),
            cmd
        ));
        json_list.push(format!(
            "{{\"pid\":{},\"name\":{},\"user\":{},\"unit\":{},\"read_bytes_per_s\":{:.0},\"write_bytes_per_s\":{:.0},\"cmd\":{}}}",
            u.pid,
            json_str(&name),
            json_str(&user),
            json_str(&unit),
            read,
            write,
            json_str(&cmd)
        ));
    }
    let msg = format!("{head} | {}", list.join(" | "));
    logger.emit(
        "alert",
        &msg,
        &format!("{base_json},\"processes\":[{}]", json_list.join(",")),
    );
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_unit_is_extracted() {
        assert_eq!(
            parse_cgroup_unit(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-brave@1.service\n"
            ),
            "app-brave@1"
        );
        assert_eq!(
            parse_cgroup_unit("0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-org.kde.konsole-1521.scope/tab(1529).scope\n"),
            "app-org.kde.konsole-1521"
        );
        assert_eq!(
            parse_cgroup_unit(&format!(
                "0::/system.slice/docker-{}.scope\n",
                "a1b2c3d4e5f6".repeat(5)
            )),
            "docker-a1b2c3d4e5f6"
        );
        assert_eq!(parse_cgroup_unit("0::/\n"), "");
        assert_eq!(
            parse_cgroup_unit("12:cpu:/x\n1:name=systemd:/system.slice/cron.service\n"),
            "cron"
        );
        assert_eq!(parse_cgroup_unit(""), "");
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
}
