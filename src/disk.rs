//! Disk handling: automatic detection of the physical disk and IO
//! utilization from `/proc/diskstats`.

use std::fs;
use std::path::PathBuf;

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
pub fn detect_device() -> Option<String> {
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

/// Reads a small sysfs attribute, trimmed.
fn sysfs(path: &str) -> Option<String> {
    fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// One line per physical disk for `--list-devices`: name, size, type, model
/// and a marker for the disk that holds `/` (the `auto` choice).
pub fn list_devices() -> Vec<String> {
    let auto = detect_device();
    let mut names: Vec<String> = fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("device").exists())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
        .iter()
        .map(|n| {
            let sectors: u64 = sysfs(&format!("/sys/block/{n}/size"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let kind = match sysfs(&format!("/sys/block/{n}/queue/rotational")).as_deref() {
                Some("1") => "HDD",
                Some("0") => "SSD",
                _ => "?",
            };
            let model = sysfs(&format!("/sys/block/{n}/device/model")).unwrap_or_default();
            let mark = if auto.as_deref() == Some(n) {
                "  (auto)"
            } else {
                ""
            };
            format!(
                "{n:<12} {:>8.1} GiB  {kind}  {model}{mark}",
                sectors as f64 * 512.0 / (1u64 << 30) as f64
            )
        })
        .collect()
}

/// Cumulative counters of a block device from `/proc/diskstats`.
#[derive(Clone, Copy, Default, Debug, PartialEq)]
pub struct DiskStats {
    pub reads: u64,
    pub read_sectors: u64,
    /// Total time (ms) spent by reads, summed over all requests.
    pub read_ms: u64,
    pub writes: u64,
    pub write_sectors: u64,
    pub write_ms: u64,
    /// Time (ms) during which the disk had IO in flight (basis of `%util`).
    pub io_ms: u64,
    /// Weighted time (ms) of IO in flight (basis of the queue depth).
    pub weighted_ms: u64,
}

/// Extracts the counters of the given device from `/proc/diskstats`.
///
/// Each line is `major minor name field1 field2 ...`, so statistics field N
/// is at index N + 2 of the line.
fn parse_stats(diskstats: &str, device: &str) -> Option<DiskStats> {
    diskstats.lines().find_map(|line| {
        let p: Vec<&str> = line.split_whitespace().collect();
        if p.get(2) != Some(&device) {
            return None;
        }
        let f = |i: usize| p.get(i)?.parse::<u64>().ok();
        Some(DiskStats {
            reads: f(3)?,
            read_sectors: f(5)?,
            read_ms: f(6)?,
            writes: f(7)?,
            write_sectors: f(9)?,
            write_ms: f(10)?,
            io_ms: f(12)?,
            weighted_ms: f(13)?,
        })
    })
}

pub fn read_stats(device: &str) -> Option<DiskStats> {
    parse_stats(&fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// Utilization percentage (equivalent to `%util` in `iostat -x`):
/// the fraction of elapsed time during which the disk was busy.
pub fn utilization(delta_ticks_ms: u64, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    (delta_ticks_ms as f64 / elapsed_ms as f64 * 100.0).min(100.0)
}

/// Disk metrics over one sampling period.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// `%util`: how much of the time the disk was busy. This is not
    /// throughput: a disk can be 100% busy doing little work.
    pub util: f64,
    /// Average time (ms) per completed request, queueing included
    /// (`await` in `iostat -x`); `None` when nothing completed.
    pub await_ms: Option<f64>,
    /// Average number of requests in flight (`aqu-sz` in `iostat -x`).
    pub queue_depth: f64,
    pub read_bytes_per_s: f64,
    pub write_bytes_per_s: f64,
}

/// Computes the metrics from two counter snapshots taken `elapsed_ms` apart.
pub fn metrics(prev: &DiskStats, now: &DiskStats, elapsed_ms: u64) -> Metrics {
    let d = |a: u64, b: u64| a.saturating_sub(b);
    let secs = (elapsed_ms as f64 / 1000.0).max(0.001);
    let ios = d(now.reads, prev.reads) + d(now.writes, prev.writes);
    let io_time = d(now.read_ms, prev.read_ms) + d(now.write_ms, prev.write_ms);
    Metrics {
        util: utilization(d(now.io_ms, prev.io_ms), elapsed_ms),
        await_ms: (ios > 0).then(|| io_time as f64 / ios as f64),
        queue_depth: d(now.weighted_ms, prev.weighted_ms) as f64 / (secs * 1000.0),
        read_bytes_per_s: d(now.read_sectors, prev.read_sectors) as f64 * 512.0 / secs,
        write_bytes_per_s: d(now.write_sectors, prev.write_sectors) as f64 * 512.0 / secs,
    }
}

/// IO pressure from `/proc/pressure/io` (PSI): percentage of time in the last
/// 10 s in which tasks were stalled waiting for IO.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pressure {
    /// At least one task was stalled.
    pub some_avg10: f64,
    /// All non-idle tasks were stalled at the same time.
    pub full_avg10: f64,
}

fn parse_pressure(text: &str) -> Option<Pressure> {
    let avg10 = |kind: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(kind)?.trim_start().strip_prefix("avg10="))
            .and_then(|v| v.split_whitespace().next()?.parse::<f64>().ok())
    };
    Some(Pressure {
        some_avg10: avg10("some")?,
        full_avg10: avg10("full")?,
    })
}

/// Reads the IO pressure; `None` on kernels without PSI.
pub fn read_pressure() -> Option<Pressure> {
    parse_pressure(&fs::read_to_string("/proc/pressure/io").ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_are_read_from_the_right_device() {
        let s = "   8       0 sda 100 0 800 50 200 0 1600 70 0 4242 120 0 0 0 0 0 0\n\
                 \x20  8       1 sda1 1 0 8 5 2 0 16 7 0 99 12 0 0 0 0 0 0\n";
        let a = parse_stats(s, "sda").unwrap();
        assert_eq!((a.reads, a.read_sectors, a.read_ms), (100, 800, 50));
        assert_eq!((a.writes, a.write_sectors, a.write_ms), (200, 1600, 70));
        assert_eq!((a.io_ms, a.weighted_ms), (4242, 120));
        assert_eq!(parse_stats(s, "sda1").unwrap().io_ms, 99);
        assert_eq!(parse_stats(s, "nvme0n1"), None);
    }

    #[test]
    fn metrics_follow_iostat_definitions() {
        let a = DiskStats::default();
        let b = DiskStats {
            reads: 10,
            read_sectors: 2048,
            read_ms: 100,
            writes: 10,
            write_sectors: 4096,
            write_ms: 300,
            io_ms: 900,
            weighted_ms: 3000,
        };
        let m = metrics(&a, &b, 2000);
        assert_eq!(m.util, 45.0);
        assert_eq!(m.await_ms, Some(20.0));
        assert_eq!(m.queue_depth, 1.5);
        assert_eq!(m.read_bytes_per_s, 512.0 * 1024.0);
        assert_eq!(m.write_bytes_per_s, 1024.0 * 1024.0);
        assert_eq!(metrics(&b, &b, 2000).await_ms, None);
    }

    #[test]
    fn pressure_is_parsed() {
        let t = "some avg10=1.50 avg60=0.50 avg300=0.10 total=123\n\
                 full avg10=0.25 avg60=0.10 avg300=0.00 total=45\n";
        assert_eq!(
            parse_pressure(t),
            Some(Pressure {
                some_avg10: 1.5,
                full_avg10: 0.25
            })
        );
        assert_eq!(parse_pressure(""), None);
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
}
