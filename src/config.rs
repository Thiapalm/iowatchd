//! Monitor configuration: defaults and the `key = value` file parser.

use std::path::PathBuf;

/// Monitor parameters. Defaults are in [`Config::default`]; they can be
/// overridden by a `key = value` file (see `config.conf`).
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Device name in `/proc/diskstats` (e.g. `sda`, `nvme0n1`) or `auto`
    /// to detect the physical disk that holds the root partition.
    pub device: String,
    /// IO utilization (%) at or above which processes are logged.
    pub threshold: f64,
    /// IO utilization (%) below which IO is considered back to normal.
    /// Keeping it below `threshold` avoids flapping between states.
    pub clear_threshold: f64,
    /// Seconds between disk samples.
    pub interval: u64,
    /// Consecutive samples above the threshold required to trigger.
    pub trigger_samples: u32,
    /// Window (s) used to measure per-process IO once triggered.
    pub process_window: u64,
    /// How many processes to list per report.
    pub top_n: usize,
    /// Minimum interval (s) between reports while IO stays high.
    pub cooldown: u64,
    /// Log file in addition to stdout/journald (`None` disables it).
    pub log_file: Option<PathBuf>,
    /// Maximum log file size (bytes) before rotating; 0 disables the built-in
    /// rotation (useful when logrotate manages the file).
    pub log_max_bytes: u64,
    /// Log format: plain text or one JSON object per line.
    pub log_json: bool,
    /// Shell command run on every alert and recovery (`None` disables it).
    /// It receives the details in `IOWATCHD_*` environment variables.
    pub notify_command: Option<String>,
    /// `--report` also reads the disk's SMART data through `smartctl`.
    pub smart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device: "auto".into(),
            threshold: 80.0,
            clear_threshold: 70.0,
            interval: 2,
            trigger_samples: 2,
            process_window: 1,
            top_n: 5,
            cooldown: 10,
            log_file: Some(PathBuf::from("/var/log/iowatchd.log")),
            log_max_bytes: 5 * 1024 * 1024,
            log_json: false,
            notify_command: None,
            smart: true,
        }
    }
}

impl Config {
    /// Applies the contents of a configuration file on top of the current
    /// values (so several files can be layered). Invalid lines are ignored and
    /// returned as warnings. Call [`Config::normalize`] when all is applied.
    pub fn apply(&mut self, text: &str) -> Vec<String> {
        let cfg = self;
        let mut warnings = Vec::new();

        for (n, raw) in text.lines().enumerate() {
            // Strip comments and whitespace.
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                warnings.push(format!("line {}: expected 'key = value'", n + 1));
                continue;
            };
            let (key, value) = (key.trim(), value.trim());

            let ok = match key {
                "device" => {
                    cfg.device = value.to_string();
                    !value.is_empty()
                }
                "threshold" => value.parse().map(|v| cfg.threshold = v).is_ok(),
                "clear_threshold" => value.parse().map(|v| cfg.clear_threshold = v).is_ok(),
                "interval" => value.parse().map(|v| cfg.interval = v).is_ok(),
                "trigger_samples" => value.parse().map(|v| cfg.trigger_samples = v).is_ok(),
                "process_window" => value.parse().map(|v| cfg.process_window = v).is_ok(),
                "top_n" => value.parse().map(|v| cfg.top_n = v).is_ok(),
                "cooldown" => value.parse().map(|v| cfg.cooldown = v).is_ok(),
                "log_file" => {
                    cfg.log_file = if value.is_empty() {
                        None
                    } else {
                        Some(value.into())
                    };
                    true
                }
                "notify_command" => {
                    cfg.notify_command = (!value.is_empty()).then(|| value.to_string());
                    true
                }
                "smart" => match value {
                    "auto" => {
                        cfg.smart = true;
                        true
                    }
                    "off" => {
                        cfg.smart = false;
                        true
                    }
                    _ => false,
                },
                "log_max_mb" => value
                    .parse::<u64>()
                    .map(|v| cfg.log_max_bytes = v * 1024 * 1024)
                    .is_ok(),
                "log_format" => match value {
                    "text" => {
                        cfg.log_json = false;
                        true
                    }
                    "json" => {
                        cfg.log_json = true;
                        true
                    }
                    _ => false,
                },
                _ => {
                    warnings.push(format!("line {}: unknown key '{}'", n + 1, key));
                    true
                }
            };
            if !ok {
                warnings.push(format!("line {}: invalid value for '{}'", n + 1, key));
            }
        }

        warnings
    }

    /// Enforces safe values (avoids a loop without sleeping or inverted hysteresis).
    pub fn normalize(&mut self) {
        self.interval = self.interval.max(1);
        self.process_window = self.process_window.max(1);
        self.trigger_samples = self.trigger_samples.max(1);
        if self.clear_threshold > self.threshold {
            self.clear_threshold = self.threshold;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Config, Vec<String>) {
        let mut cfg = Config::default();
        let warnings = cfg.apply(text);
        cfg.normalize();
        (cfg, warnings)
    }

    #[test]
    fn later_files_override_earlier_ones() {
        let mut cfg = Config::default();
        cfg.apply("threshold = 90\ntop_n = 3\n");
        cfg.apply("threshold = 60\n");
        assert_eq!((cfg.threshold, cfg.top_n), (60.0, 3));
    }

    #[test]
    fn reads_notify_command() {
        let (c, w) = parse("notify_command = /usr/local/bin/hook --flag\n");
        assert_eq!(
            c.notify_command.as_deref(),
            Some("/usr/local/bin/hook --flag")
        );
        assert!(w.is_empty());
        let (c, _) = parse("notify_command =\n");
        assert!(c.notify_command.is_none());
    }

    #[test]
    fn reads_log_format() {
        let (c, w) = parse("log_format = json\nlog_max_mb = 0\n");
        assert!(c.log_json && w.is_empty());
        assert_eq!(c.log_max_bytes, 0);
        let (c, w) = parse("log_format = xml\n");
        assert!(!c.log_json);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn applies_values_and_warns_about_errors() {
        let (c, w) =
            parse("device = nvme0n1\nthreshold=90 # x\ninterval = abc\nfoo = 1\nlog_file =\n");
        assert_eq!(c.device, "nvme0n1");
        assert_eq!(c.threshold, 90.0);
        assert_eq!(c.interval, 2);
        assert!(c.log_file.is_none());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn fixes_inverted_hysteresis() {
        let (c, _) = parse("threshold = 50\nclear_threshold = 70\ninterval = 0\n");
        assert_eq!(c.clear_threshold, 50.0);
        assert_eq!(c.interval, 1);
    }

    #[test]
    fn normalize_clamps_unsafe_values() {
        let (cfg, _) = parse("interval = 0\nprocess_window = 0\ntrigger_samples = 0\n");
        assert_eq!(
            (cfg.interval, cfg.process_window, cfg.trigger_samples),
            (1, 1, 1)
        );
    }

    #[test]
    fn reads_remaining_keys() {
        let (cfg, w) = parse(
            "device = sda\ntop_n = 9\ncooldown = 30\nlog_file =\nlog_max_mb = 0\nsmart = off\n",
        );
        assert_eq!(cfg.device, "sda");
        assert_eq!((cfg.top_n, cfg.cooldown, cfg.log_max_bytes), (9, 30, 0));
        assert_eq!(cfg.log_file, None);
        assert!(!cfg.smart);
        assert!(w.is_empty(), "{w:?}");
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let (cfg, w) = parse("# c\n\n  threshold = 90  # inline\n");
        assert_eq!(cfg.threshold, 90.0);
        assert!(w.is_empty());
    }
}
