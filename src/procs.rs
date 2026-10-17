//! Per-process IO: snapshots of `/proc/<pid>/io`, ranking and the alert report.

use std::collections::HashMap;
use std::fs;
use std::time::Instant;

use crate::config::Config;
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
fn human_rate(bytes_per_s: f64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes_per_s >= MB {
        format!("{:.1}MB/s", bytes_per_s / MB)
    } else {
        format!("{:.0}KB/s", bytes_per_s / 1024.0)
    }
}

/// Measures per-process IO over the configured window and writes the report to the log.
pub fn report_top_processes(cfg: &Config, logger: &Logger, util: f64) {
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

#[cfg(test)]
mod tests {
    use super::*;

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
