use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::NaiveDate;
use sha2::{Digest, Sha256};
use tracing::{debug, trace, warn};

use crate::{
    config::Cfg,
    dist::{
        Channel, ToolchainDesc,
        manifest::{Manifest, ManifestWithHash},
    },
    errors::RustupError,
    process::Process,
    tuf::{TufRepository, Verification},
    utils,
};

const UPDATE_HASH_LEN: usize = 20;

// Expand ToolChainDesc here so we don't sprinkle code further into the top level codebase
// We implement a manifests v3 which directs to the new pathing used for channels
impl ToolchainDesc {
    // Added impl for TUF specific url-mapping changes for the new channel dist paths
    /*
        This implementation currently gives us a layout that looks like this:
        
        ├── channels/
        │   ├── beta/
        │   │   ├── 1.75-beta-2023-11-13.toml
        │   │   ├── 1.75-beta-2023-11-19.toml
        │   │   ├── 1.75-beta-2023-11-21.toml
        │   │   ├── 1.75-beta-2023-11-29.toml
        │   │   ├── 1.75-beta-2023-12-02.toml
        │   │   └── ... (1195 more)
        │   ├── current/
        │   │   ├── beta.toml
        │   │   ├── nightly.toml
        │   │   └── stable.toml
        │   ├── nightly/
        │   │   ├── 2018/
        │   │   │   ├── 01-01/
        │   │   │   │   └── nightly.toml
        │   │   │   ├── 01-02/
        │   │   │   │   ├── beta.toml
        │   │   │   │   └── nightly.toml
        │   │   │   ├── 01-03/
        │   │   │   │   └── nightly.toml
        │   │   │   ├── 01-04/
        │   │   │   │   ├── nightly.toml
        │   │   │   │   └── stable.toml
        │   │   │   ├── 01-05/
        │   │   │   │   └── nightly.toml
        │   │   │   └── ... (314 more)
        │   └── stable/
        │       ├── 1.10.0.toml
        │       ├── 1.11.0.toml
        │       ├── 1.12.0.toml
        │       ├── 1.12.1.toml
        │       ├── 1.13.0.toml
        │       └── ... (178 more)

        TODO: we have an open question here on how/where we want to duplicate manifests.
            to support dated channel requests for the existing format, we either need to:
            1. duplicate stable/beta files as version.toml and date.toml 
            2. We use the nightly role path for dated lookups; but this exposes stable role
                to nightly role and breaks the security boundary of roles for the folders for
                channels matching.
    */
    pub(crate) fn manifest_v3_url(&self, dist_root: &str, process: &Process) -> Result<String> {
        let do_manifest_staging = process.var("RUSTUP_STAGED_MANIFEST").is_ok();
        trace!("{}, {}", &self.channel, &self.target);

        match (self.date.as_ref(), do_manifest_staging) {
            (None, false) => {
                match &self.channel {
                    Channel::Nightly | Channel::Beta | Channel::Stable => Ok(format!(
                        "{}/channels/current/{}.toml",
                        dist_root, self.channel
                    )),
                    Channel::Version(version) => {
                        // TODO: Is this good enough?
                        if !version.pre.is_empty() {
                            Ok(format!("{}/channels/beta/{}.toml", dist_root, self.channel))
                        } else {
                            Ok(format!(
                                "{}/channels/stable/{}.toml",
                                dist_root, self.channel
                            ))
                        }
                    }
                }
            }
            (Some(date), false) => {
                let date = NaiveDate::parse_from_str(date, "%Y-%m-%d")
                    .with_context(|| format!("invalid date '{date}', expected yyyy-mm-dd"))?;
                Ok(format!(
                    "{}/channels/nightly/{}/{}.toml",
                    dist_root,
                    date.format("%Y/%m-%d"),
                    self.channel
                ))
            }
            (None, true) => Ok(format!(
                "{}/channels/staging/{}.toml",
                dist_root, self.channel
            )),
            (Some(_), true) => panic!("not a real-world case"),
        }
    }
}

pub(crate) async fn dl_v2_manifest(
    update_hash: Option<&Path>,
    toolchain: &ToolchainDesc,
    cfg: &Cfg<'_>,
) -> Result<Option<ManifestWithHash>> {
    let location = match &cfg.tuf.dist_server {
        Some(server) => {
            trace!(server, "using RUSTUP_TUF_DIST_SERVER as TUF dist location");
            server.clone()
        }
        None => {
            trace!(
                dist_root = cfg.dist_root_url,
                "RUSTUP_TUF_DIST_SERVER unset, using dist root as TUF dist location"
            );
            cfg.dist_root_url.clone()
        }
    };
    let target = toolchain.manifest_v3_url("", cfg.process)?;
    let target = target.trim_start_matches('/');

    debug!(
        location,
        target,
        toolchain = %toolchain,
        update_hash = ?update_hash,
        "fetching channel manifest through TUF"
    );
    let mut repo = TufRepository::open(&cfg.tuf, &location, cfg.process).await?;
    repo.verify().await?;
    let (bytes, verification) = repo.fetch_target(target).await?;
    match verification {
        Verification::Verified => debug!(target, "TUF verification passed"),
        Verification::Skipped => warn!(target, "TUF verification skipped"),
    }

    let hash = faster_hex::hex_string(&Sha256::digest(&bytes));
    let partial_hash: String = hash.chars().take(UPDATE_HASH_LEN).collect();
    trace!(
        target,
        len = bytes.len(),
        hash,
        partial_hash,
        "hashed TUF channel manifest"
    );

    if let Some(hash_file) = update_hash {
        if utils::is_file(hash_file) {
            if let Ok(contents) = utils::read_file("update hash", hash_file) {
                if contents == partial_hash {
                    debug!(target, file = %hash_file.display(), "update hash matches, skipping manifest");
                    return Ok(None);
                }
                trace!(target, file = %hash_file.display(), contents, "update hash differs");
            } else {
                warn!(
                    "can't read update hash {}, can't skip update",
                    hash_file.display()
                );
            }
        } else {
            debug!(file = %hash_file.display(), "no update hash file found");
        }
    }

    let manifest_str = String::from_utf8(bytes)
        .with_context(|| format!("channel manifest '{target}' is not valid UTF-8"))?;
    let manifest = Manifest::parse(&manifest_str).with_context(|| RustupError::ParsingFile {
        name: "manifest",
        path: PathBuf::from(target),
    })?;
    debug!(
        target,
        date = manifest.date,
        version = ?manifest.get_rust_version().ok(),
        packages = manifest.packages.len(),
        "parsed TUF channel manifest"
    );

    Ok(Some(ManifestWithHash {
        manifest,
        hash: partial_hash,
    }))
}
