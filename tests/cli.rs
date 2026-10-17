//! Integration tests: run the real binary and check exit codes, output and signals.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_iowatchd");

/// First real-looking block device in /proc/diskstats, if any
/// (CI containers may not have one, in which case dependent tests are skipped).
fn any_device() -> Option<String> {
    std::fs::read_to_string("/proc/diskstats")
        .ok()?
        .lines()
        .filter_map(|l| l.split_whitespace().nth(2).map(String::from))
        .find(|n| !n.starts_with("loop") && !n.starts_with("ram") && !n.starts_with("zram"))
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(BIN)
        .args(args)
        .output()
        .expect("failed to run iowatchd");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn help_and_version_exit_zero() {
    let (code, out, _) = run(&["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("Usage: iowatchd"));

    let (code, out, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("iowatchd "));
}

#[test]
fn usage_errors_exit_two() {
    for args in [
        &["--bogus"][..],
        &["stray"],
        &["-t", "500"],
        &["-i", "0"],
        &["-d"],
    ] {
        let (code, _, err) = run(args);
        assert_eq!(code, 2, "args {args:?}");
        assert!(err.contains("--help"), "args {args:?}");
    }
}

#[test]
fn runtime_errors_exit_one() {
    let (code, _, err) = run(&["-d", "no-such-disk", "--once"]);
    assert_eq!(code, 1);
    assert!(err.contains("not found"));

    let (code, _, err) = run(&["-c", "/no/such/file.conf", "--once"]);
    assert_eq!(code, 1);
    assert!(err.contains("could not read"));
}

#[test]
fn once_prints_a_sample_in_text_and_json() {
    let Some(dev) = any_device() else { return };

    let (code, out, _) = run(&["-d", &dev, "-i", "1", "-t", "100", "--once"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("IO=") && out.contains(&dev), "{out}");

    let (code, out, _) = run(&["-d", &dev, "-i", "1", "-t", "100", "--once", "--json"]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with('{') && out.contains("\"event\":\"sample\""),
        "{out}"
    );
}

#[test]
fn check_config_reports_problems() {
    let Some(dev) = any_device() else { return };
    let dir = std::env::temp_dir().join(format!("iowatchd-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let good = dir.join("good.conf");
    std::fs::write(&good, format!("device = {dev}\nthreshold = 70\n")).unwrap();
    let (code, out, _) = run(&["-c", good.to_str().unwrap(), "--check-config"]);
    assert_eq!(code, 0);
    assert!(out.contains("threshold=70") && out.contains("configuration OK"));

    let bad = dir.join("bad.conf");
    std::fs::write(&bad, format!("device = {dev}\nnonsense = 1\n")).unwrap();
    let (code, _, err) = run(&["-c", bad.to_str().unwrap(), "--check-config"]);
    assert_eq!(code, 1);
    assert!(err.contains("unknown key"));

    let _ = std::fs::remove_dir_all(dir);
}

/// Sends `signal` to a running daemon and returns its stdout and exit code.
fn run_daemon_and_signal(dev: &str, signal: &str, wait_for: &str) -> (Option<i32>, String) {
    let mut child = Command::new(BIN)
        .args(["-d", dev, "-i", "1", "-q"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    // -q hides stdout, so give the daemon a moment to install its handlers.
    std::thread::sleep(Duration::from_millis(500));
    let status = Command::new("kill")
        .args([&format!("-{signal}"), &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(status.success());

    let start = Instant::now();
    let code = loop {
        if let Some(st) = child.try_wait().unwrap() {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(5) {
            child.kill().unwrap();
            panic!("daemon did not stop after SIG{signal} ({wait_for})");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    (code, out)
}

#[test]
fn sigterm_and_sigint_stop_cleanly() {
    let Some(dev) = any_device() else { return };
    for sig in ["TERM", "INT"] {
        let (code, _) = run_daemon_and_signal(&dev, sig, "clean stop");
        assert_eq!(code, Some(0), "SIG{sig}");
    }
}

#[test]
fn version_mentions_license_and_repository() {
    let (code, out, _) = run(&["--version"]);
    assert_eq!(code, 0);
    assert!(out.contains("MIT") && out.contains("github.com/Thiapalm/iowatchd"));
}

#[test]
fn grouped_flags_and_attached_values_work() {
    let Some(dev) = any_device() else { return };
    let (code, out, _) = run(&[&format!("-jod{dev}"), "-i1", "-t100"]);
    assert_eq!(code, 0);
    assert!(out.contains("\"event\""), "{out}");
    let (code, _, err) = run(&["--thresold=5"]);
    assert_eq!(code, 2);
    assert!(err.contains("did you mean '--threshold'"), "{err}");
}

#[test]
fn list_devices_succeeds() {
    let (code, _, _) = run(&["--list-devices"]);
    assert_eq!(code, 0);
}

#[test]
fn env_config_and_dropins_are_layered() {
    let Some(dev) = any_device() else { return };
    let dir = std::env::temp_dir().join(format!("iowatchd-env-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("main.conf.d")).unwrap();
    std::fs::write(
        dir.join("main.conf"),
        format!("device = {dev}\nthreshold = 90\ntop_n = 3\n"),
    )
    .unwrap();
    std::fs::write(dir.join("main.conf.d/10-local.conf"), "threshold = 55\n").unwrap();

    let out = Command::new(BIN)
        .arg("--check-config")
        .env("IOWATCHD_CONFIG", dir.join("main.conf"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("threshold=55") && text.contains("top_n=3"),
        "{text}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn closed_pipe_kills_quietly_with_sigpipe() {
    use std::os::unix::process::ExitStatusExt;
    let Some(dev) = any_device() else { return };
    let mut child = Command::new(BIN)
        .args(["-d", &dev, "--once", "-i", "1", "-t", "100"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(13), "{status:?}");
}

#[test]
fn sigusr1_logs_status() {
    let Some(dev) = any_device() else { return };
    let dir = std::env::temp_dir().join(format!("iowatchd-usr1-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let conf = dir.join("t.conf");
    std::fs::write(&conf, format!("device = {dev}\nlog_file =\ninterval = 1\n")).unwrap();

    let mut child = Command::new(BIN)
        .args(["-c", conf.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_millis(500));
    for sig in ["USR1", "TERM"] {
        assert!(Command::new("kill")
            .args([&format!("-{sig}"), &child.id().to_string()])
            .status()
            .unwrap()
            .success());
        std::thread::sleep(Duration::from_millis(300));
    }
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    assert_eq!(child.wait().unwrap().code(), Some(0));
    assert!(out.contains("status: device="), "{out}");

    let _ = std::fs::remove_dir_all(dir);
}
