//! User notification hook: runs the configured `notify_command` when an alert
//! starts or IO recovers. The command runs in the background so the monitor
//! loop is never blocked.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A hook that runs longer than this is killed.
const HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Information passed to the hook as environment variables.
pub struct HookEvent<'a> {
    /// `alert` or `normal`.
    pub event: &'a str,
    pub device: &'a str,
    pub util: f64,
    /// The same human-readable text that was logged.
    pub message: &'a str,
}

/// Runs `command` through `sh -c` with:
/// `IOWATCHD_EVENT`, `IOWATCHD_DEVICE`, `IOWATCHD_UTIL` and `IOWATCHD_MESSAGE`.
/// Failures are reported on stderr and never stop the monitor.
pub fn run_hook(command: &str, ev: &HookEvent) {
    let spawned = Command::new("sh")
        .arg("-c")
        .arg(command)
        .env("IOWATCHD_EVENT", ev.event)
        .env("IOWATCHD_DEVICE", ev.device)
        .env("IOWATCHD_UTIL", format!("{:.0}", ev.util))
        .env("IOWATCHD_MESSAGE", ev.message)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(c) => c,
        Err(e) => return eprintln!("warning: could not run notify_command: {e}"),
    };
    // Reap the child (no zombies) and enforce the timeout off the main thread.
    std::thread::spawn(move || {
        let start = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) if start.elapsed() > HOOK_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_receives_the_environment() {
        let out = std::env::temp_dir().join(format!("iowatchd-hook-{}", std::process::id()));
        let cmd = format!(
            "echo \"$IOWATCHD_EVENT $IOWATCHD_DEVICE $IOWATCHD_UTIL $IOWATCHD_MESSAGE\" > {}",
            out.display()
        );
        run_hook(
            &cmd,
            &HookEvent {
                event: "alert",
                device: "sda",
                util: 97.4,
                message: "hello world",
            },
        );
        for _ in 0..50 {
            if out.exists() {
                std::thread::sleep(Duration::from_millis(100));
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let text = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        assert_eq!(text.trim(), "alert sda 97 hello world");
    }
}
