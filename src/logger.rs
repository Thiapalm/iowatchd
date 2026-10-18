//! Logging: stdout (journald) plus a log file with simple rotation, in plain
//! text or JSON lines.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Set once a log-file write failure has been reported, to avoid repeating it.
static FILE_WARNED: AtomicBool = AtomicBool::new(false);

pub struct Logger {
    pub file: Option<PathBuf>,
    pub max_bytes: u64,
    /// Emit one JSON object per line instead of plain text.
    pub json: bool,
    /// Do not print to stdout (the log file still receives entries).
    pub quiet: bool,
}

/// Escapes a string as a JSON string literal (including the quotes).
pub fn json_str(s: &str) -> String {
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
    pub fn log(&self, kind: &str, text: &str) {
        self.emit(kind, text, "");
    }

    /// Writes an event to stdout (captured by journald, which already adds a
    /// timestamp) and to the log file (with its own timestamp).
    ///
    /// `fields` holds extra pre-rendered JSON members (without the braces),
    /// e.g. `"device":"sda","util":94`; they are only used in JSON mode.
    pub fn emit(&self, kind: &str, text: &str, fields: &str) {
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
            Err(e) => {
                if !FILE_WARNED.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "warning: could not write to {}: {e} (further file errors are not repeated)",
                        path.display()
                    );
                }
            }
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
    format_time(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    )
}

/// Formats a Unix time in the same local format as [`timestamp`].
pub fn format_time(secs: i64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
    }

    fn temp_log(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("iowatchd-{name}-{}.log", std::process::id()))
    }

    fn logger(file: PathBuf, max_bytes: u64, json: bool) -> Logger {
        Logger {
            file: Some(file),
            max_bytes,
            json,
            quiet: true,
        }
    }

    #[test]
    fn text_lines_have_a_timestamp_prefix() {
        let path = temp_log("text");
        let _ = fs::remove_file(&path);
        logger(path.clone(), 0, false).emit("alert", "hello", "\"x\":1");
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let line = text.trim_end();
        assert!(line.ends_with(" hello"), "{line}");
        assert_eq!(line.len(), "YYYY-MM-DD HH:MM:SS hello".len());
    }

    #[test]
    fn json_lines_include_extra_fields() {
        let path = temp_log("json");
        let _ = fs::remove_file(&path);
        let l = logger(path.clone(), 0, true);
        l.emit("alert", "a \"b\"", "\"util\":94");
        l.log("started", "go");
        let text = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("{\"time\":\""));
        assert!(lines[0].contains("\"event\":\"alert\",\"message\":\"a \\\"b\\\"\",\"util\":94}"));
        assert!(lines[1].ends_with("\"message\":\"go\"}"));
    }

    #[test]
    fn file_rotates_to_dot_one_past_the_limit() {
        let path = temp_log("rotate");
        let old = PathBuf::from(format!("{}.1", path.display()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&old);
        let l = logger(path.clone(), 50, false);
        l.log("a", &"x".repeat(60));
        l.log("b", "fresh");
        let current = fs::read_to_string(&path).unwrap();
        let rotated = fs::read_to_string(&old).unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&old);
        assert!(rotated.contains(&"x".repeat(60)));
        assert!(current.contains("fresh") && !current.contains('x'));
    }

    #[test]
    fn zero_max_bytes_disables_rotation() {
        let path = temp_log("norotate");
        let old = PathBuf::from(format!("{}.1", path.display()));
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(&old);
        let l = logger(path.clone(), 0, false);
        l.log("a", &"x".repeat(200));
        l.log("b", "more");
        let exists = old.exists();
        let lines = fs::read_to_string(&path).unwrap().lines().count();
        let _ = fs::remove_file(&path);
        assert!(!exists);
        assert_eq!(lines, 2);
    }

    #[test]
    fn format_time_has_the_expected_shape() {
        let t = format_time(0);
        let b = t.as_bytes();
        assert_eq!(t.len(), 19, "{t}");
        assert!(b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':' && b[16] == b':');
        assert_ne!(format_time(0), format_time(86_400 * 3));
    }
}
