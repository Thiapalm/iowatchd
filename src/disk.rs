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

pub fn read_io_ticks(device: &str) -> Option<u64> {
    parse_io_ticks(&fs::read_to_string("/proc/diskstats").ok()?, device)
}

/// Utilization percentage (equivalent to `%util` in `iostat -x`):
/// the fraction of elapsed time during which the disk was busy.
pub fn utilization(delta_ticks_ms: u64, elapsed_ms: u64) -> f64 {
    if elapsed_ms == 0 {
        return 0.0;
    }
    (delta_ticks_ms as f64 / elapsed_ms as f64 * 100.0).min(100.0)
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
