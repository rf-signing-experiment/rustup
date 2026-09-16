use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};
use tracing::{debug, trace, warn};

use super::{TufRepository, Verification};
use crate::{
    config::Cfg,
    dist::{
        ToolchainDesc,
        manifest::{Manifest, ManifestWithHash},
    },
    errors::RustupError,
    utils,
};

const UPDATE_HASH_LEN: usize = 20;

pub(crate) async fn dl_v2_manifest(
    update_hash: Option<&Path>,
    toolchain: &ToolchainDesc,
    cfg: &Cfg<'_>,
) -> anyhow::Result<Option<ManifestWithHash>> {
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
    let target = toolchain.manifest_v2_url("", cfg.process);
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
