use anyhow::{Context, Result};
use chrono::NaiveDate;
use tracing::trace;

use crate::{
    dist::{Channel, ChannelToolchainName},
    process::Process,
};

// The TUF-specific channel layout lives here rather than next to the v1 and
// v2 URL builders in `dist`, so the TUF code stays in one place.
impl ChannelToolchainName {
    /// The URL of this toolchain's channel manifest in the TUF target layout
    /// ("manifest v3"), rooted at `dist_root`.
    ///
    /// The layout is:
    ///
    /// ```text
    /// channels/
    /// ├── archive/
    /// │   └── 2026/
    /// │       ├── 06-01/
    /// │       │   └── nightly.toml
    /// │       ├── 09-03/
    /// │       │   └── stable.toml
    /// │       ├── 09-11/
    /// │       │   ├── beta.toml
    /// │       │   └── nightly.toml
    /// │       └── ...
    /// ├── beta/
    /// │   ├── 1.75-beta-2023-11-13.toml
    /// │   └── ...
    /// ├── current/
    /// │   ├── beta.toml
    /// │   ├── nightly.toml
    /// │   └── stable.toml
    /// └── stable/
    ///     ├── 1.10.0.toml
    ///     └── ...
    /// ```
    ///
    /// Dated requests for the named channels resolve under
    /// `archive/<year>/<month-day>/`, which the repository signs with one
    /// delegated role per year.
    pub(crate) fn manifest_v3_url(&self, dist_root: &str, process: &Process) -> Result<String> {
        let do_manifest_staging = process.var("RUSTUP_STAGED_MANIFEST").is_ok();
        trace!(
            do_manifest_staging,
            channel = %self.channel,
            target = %self.target,
            "building v3 manifest url"
        );

        Ok(match (self.date.as_ref(), do_manifest_staging) {
            (None, false) => match &self.channel {
                Channel::Nightly | Channel::Beta | Channel::Stable => {
                    format!("{}/channels/current/{}.toml", dist_root, self.channel)
                }
                // A pre-release version is a beta; everything else is a
                // stable release.
                Channel::Version(version) if !version.pre.is_empty() => {
                    format!("{}/channels/beta/{}.toml", dist_root, self.channel)
                }
                Channel::Version(_) => {
                    format!("{}/channels/stable/{}.toml", dist_root, self.channel)
                }
            },
            (Some(date_str), false) => {
                let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                    .with_context(|| format!("invalid date '{date_str}', expected yyyy-mm-dd"))?;

                match &self.channel {
                    Channel::Nightly | Channel::Beta | Channel::Stable => {
                        format!(
                            "{}/channels/archive/{}/{}.toml",
                            dist_root,
                            date.format("%Y/%m-%d"),
                            self.channel
                        )
                    }
                    // A pre-release version is a beta; everything else is a
                    // stable release.
                    Channel::Version(version) if !version.pre.is_empty() => {
                        format!("{}/channels/beta/{}-{}.toml", dist_root, version, date)
                    }
                    Channel::Version(version) => {
                        format!("{}/channels/stable/{}.toml", dist_root, version)
                    }
                }
            }
            (None, true) => format!("{}/channels/staging/{}.toml", dist_root, self.channel),
            (Some(_), true) => panic!("not a real-world case"),
        })
    }
}
