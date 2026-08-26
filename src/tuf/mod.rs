//! Settings for TUF (The Update Framework) signature validation.
//!
//! This module only declares and reads the configuration surface; nothing in
//! rustup consumes it yet. Every setting comes from an environment variable
//! read through [`Process`], so the CLI test harness can drive it.

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{DateTime, NaiveDate, Utc};
use tracing::trace;

use crate::process::Process;

mod manifest;
mod provider;

pub(crate) use self::manifest::dl_v2_manifest;
pub(crate) use self::provider::{TufRepository, Verification};

/// TUF-related settings, resolved from the `RUSTUP_TUF_*` environment variables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TufConfig {
    /// URL of the TUF root used to validate signatures of dist files
    /// (`RUSTUP_TUF_DIST_SERVER`).
    pub dist_server: Option<String>,
    /// URL of the TUF root used to validate signatures of rustup itself
    /// (`RUSTUP_TUF_UPDATE_SERVER`).
    pub update_server: Option<String>,
    /// A local `root.json` to use instead of the one shipped with rustup
    /// (`RUSTUP_TUF_ROOT`).
    pub root: Option<PathBuf>,
    /// Directory holding the local copy of the TUF repository and key files
    /// (`RUSTUP_TUF_HOME`, default `<RUSTUP_HOME>/tuf`).
    pub home: PathBuf,
    /// Whether, and how strictly, TUF validation runs
    /// (`RUSTUP_TUF_ENABLE`, default `off`).
    pub mode: TufMode,
    /// Ignore all validation failures, including metadata expiry
    /// (`RUSTUP_TUF_IGNORE=1`).
    pub ignore_failures: bool,
    /// Ignore metadata expiry failures whose expiry is after this instant
    /// (`RUSTUP_TUF_IGNOREDATE`).
    pub ignore_expiry_after: Option<DateTime<Utc>>,
}

impl TufConfig {
    pub(crate) fn enabled(&self) -> bool {
        self.mode != TufMode::Off
    }

    /// Reads every `RUSTUP_TUF_*` variable from `process`.
    ///
    /// `rustup_dir` is the resolved `RUSTUP_HOME`, used for the default
    /// [`TufConfig::home`]. Unparsable values fall back to their defaults.
    pub fn from_env(rustup_dir: &Path, process: &Process) -> Self {
        let dist_server = process
            .var("RUSTUP_TUF_DIST_SERVER")
            .inspect(|url| trace!("`RUSTUP_TUF_DIST_SERVER` has been set to `{url}`"))
            .ok();

        let update_server = process
            .var("RUSTUP_TUF_UPDATE_SERVER")
            .inspect(|url| trace!("`RUSTUP_TUF_UPDATE_SERVER` has been set to `{url}`"))
            .ok();

        let root = process
            .var("RUSTUP_TUF_ROOT")
            .inspect(|path| trace!("`RUSTUP_TUF_ROOT` has been set to `{path}`"))
            .ok()
            .map(PathBuf::from);

        let home = match process.var("RUSTUP_TUF_HOME") {
            Ok(path) => {
                trace!("`RUSTUP_TUF_HOME` has been set to `{path}`");
                PathBuf::from(path)
            }
            Err(_) => rustup_dir.join(DEFAULT_HOME_DIR),
        };

        let mode = match process.var("RUSTUP_TUF_ENABLE") {
            Ok(s)
                if ["on", "true", "1"]
                    .iter()
                    .any(|v| s.eq_ignore_ascii_case(v)) =>
            {
                TufMode::On
            }
            Ok(s) if s.eq_ignore_ascii_case("warn") => TufMode::Warn,
            _ => TufMode::Off,
        };

        let ignore_failures = process.var("RUSTUP_TUF_IGNORE").is_ok_and(|s| s == "1");

        let ignore_expiry_after = process
            .var("RUSTUP_TUF_IGNOREDATE")
            .ok()
            .and_then(|s| parse_date_time(&s));

        Self {
            dist_server,
            update_server,
            root,
            home,
            mode,
            ignore_failures,
            ignore_expiry_after,
        }
    }
}

/// How TUF validation behaves.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TufMode {
    /// Do not touch the TUF repository at all.
    #[default]
    Off,
    /// Synchronize the TUF repository and validate signatures, failing on error.
    On,
    /// Synchronize the TUF repository over the network but skip signature
    /// validation, only reporting what would have failed.
    Warn,
}

impl TufMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::On => "on",
            Self::Warn => "warn",
        }
    }
}

impl fmt::Display for TufMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Parses either an RFC 3339 timestamp (`2026-09-16T12:00:00Z`) or a bare
/// `YYYY-MM-DD` date, which is taken as midnight UTC.
fn parse_date_time(value: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some(dt.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(value, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc())
}

/// Name of the default [`TufConfig::home`] directory under `RUSTUP_HOME`.
const DEFAULT_HOME_DIR: &str = "tuf";

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use crate::process::TestProcess;

    fn config(vars: &[(&str, &str)]) -> TufConfig {
        let vars = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect::<HashMap<_, _>>();
        let tp = TestProcess::with_vars(vars);
        TufConfig::from_env(Path::new("/rustup"), &tp.process)
    }

    #[test]
    fn defaults() {
        assert_eq!(
            config(&[]),
            TufConfig {
                dist_server: None,
                update_server: None,
                root: None,
                home: PathBuf::from("/rustup/tuf"),
                mode: TufMode::Off,
                ignore_failures: false,
                ignore_expiry_after: None,
            }
        );
    }

    #[test]
    fn all_set() {
        let cfg = config(&[
            ("RUSTUP_TUF_DIST_SERVER", "https://tuf.example.com/dist"),
            ("RUSTUP_TUF_UPDATE_SERVER", "https://tuf.example.com/rustup"),
            ("RUSTUP_TUF_ROOT", "/etc/rustup/root.json"),
            ("RUSTUP_TUF_HOME", "/var/lib/rustup-tuf"),
            ("RUSTUP_TUF_ENABLE", "warn"),
            ("RUSTUP_TUF_IGNORE", "1"),
            ("RUSTUP_TUF_IGNOREDATE", "2026-09-16T12:34:56Z"),
        ]);
        assert_eq!(
            cfg,
            TufConfig {
                dist_server: Some("https://tuf.example.com/dist".to_owned()),
                update_server: Some("https://tuf.example.com/rustup".to_owned()),
                root: Some(PathBuf::from("/etc/rustup/root.json")),
                home: PathBuf::from("/var/lib/rustup-tuf"),
                mode: TufMode::Warn,
                ignore_failures: true,
                ignore_expiry_after: "2026-09-16T12:34:56Z".parse::<DateTime<Utc>>().ok(),
            }
        );
    }

    #[test]
    fn mode_is_case_insensitive_and_lenient() {
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "on")]).mode, TufMode::On);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "ON")]).mode, TufMode::On);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "true")]).mode, TufMode::On);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "1")]).mode, TufMode::On);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "Warn")]).mode, TufMode::Warn);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "false")]).mode, TufMode::Off);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "off")]).mode, TufMode::Off);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "maybe")]).mode, TufMode::Off);
        assert_eq!(config(&[("RUSTUP_TUF_ENABLE", "")]).mode, TufMode::Off);
    }

    #[test]
    fn ignore_flag_is_only_one() {
        assert!(config(&[("RUSTUP_TUF_IGNORE", "1")]).ignore_failures);
        for other in ["0", "", "true", "yes", "2"] {
            assert!(
                !config(&[("RUSTUP_TUF_IGNORE", other)]).ignore_failures,
                "{other:?}"
            );
        }
    }

    #[test]
    fn date_parsing() {
        let midnight = "2026-09-16T00:00:00Z".parse::<DateTime<Utc>>().unwrap();
        assert_eq!(parse_date_time("2026-09-16"), Some(midnight));
        assert_eq!(parse_date_time("2026-09-16T02:00:00+02:00"), Some(midnight));
        assert_eq!(parse_date_time("yesterday"), None);
        assert_eq!(parse_date_time("2026-13-01"), None);
        assert_eq!(
            config(&[("RUSTUP_TUF_IGNOREDATE", "yesterday")]).ignore_expiry_after,
            None
        );
    }
}
