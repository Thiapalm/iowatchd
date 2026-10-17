//! Logging: stdout (journald) plus a log file with simple rotation, in plain
//! text or JSON lines.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_str("a\"b\\c\nd\te"), "\"a\\\"b\\\\c\\nd\\te\"");
        assert_eq!(json_str("\u{1}"), "\"\\u0001\"");
    }
}
