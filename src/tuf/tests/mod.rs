mod config;
mod download;
mod manifest;
mod online;

/// Fixture repository: manifests in `src/`, TUF metadata and targets in `tuf/`.
pub(crate) const REPO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/tuf/tests/repo");

/// Unreachable dist server: with TUF on, nothing is fetched from it directly.
pub(crate) const DIST_ROOT: &str = "https://dist.invalid";
