use std::{fs, path::PathBuf};

use crate::{
    dist::temp,
    download::DownloadOptions,
    errors::RustupError,
    process::Process,
    tuf::{TufConfig, TufMode},
    utils,
};
use anyhow::{Context, Result, anyhow, bail};
use futures_util::{
    FutureExt,
    future::BoxFuture,
    io::{AsyncRead, AsyncReadExt, Cursor},
};
use tracing::{debug, trace, warn};
use tuf::{
    client::{Client, Config},
    database::Database,
    metadata::{Metadata, MetadataPath, MetadataVersion, RawSignedMetadata, TargetPath},
    pouf::Pouf1,
    repository::{FileSystemRepository, RepositoryProvider},
};
use url::Url;

const METADATA_PREFIX: &str = "metadata";
const TARGETS_PREFIX: &str = "targets";
const TMP_DIR: &str = "tmp";

type Reader<'a> = Box<dyn AsyncRead + Send + Unpin + 'a>;

struct HttpRepository {
    base: Url,
    options: DownloadOptions,
    tmp_cx: temp::Context,
}

impl HttpRepository {
    fn new(base: Url, options: DownloadOptions, tmp_dir: PathBuf) -> Self {
        trace!(%base, tmp_dir = %tmp_dir.display(), ?options, "created TUF http repository");
        let tmp_cx = temp::Context::new(tmp_dir, base.as_str());
        Self {
            base,
            options,
            tmp_cx,
        }
    }

    fn url(&self, prefix: &str, components: &[String]) -> tuf::Result<Url> {
        let mut url = self.base.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                tuf::Error::IllegalArgument(format!("cannot be a base url: {}", self.base))
            })?;
            segments.pop_if_empty();
            segments.push(prefix);
            segments.extend(components);
        }
        trace!(prefix, ?components, %url, "resolved TUF url");
        Ok(url)
    }

    async fn fetch(&self, url: Url) -> Result<Vec<u8>> {
        let file = self.tmp_cx.new_file()?;
        debug!(%url, path = %file.display(), "fetching TUF file");
        self.options.start(&url, &file).download().await?;
        let bytes = fs::read(&*file)
            .with_context(|| format!("error reading TUF file '{}'", file.display()))?;
        trace!(%url, len = bytes.len(), "fetched TUF file");
        Ok(bytes)
    }
}

fn is_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<RustupError>(),
        Some(RustupError::DownloadNotExists { .. })
    )
}

impl RepositoryProvider<Pouf1> for HttpRepository {
    fn fetch_metadata<'a>(
        &'a self,
        meta_path: &MetadataPath,
        version: MetadataVersion,
    ) -> BoxFuture<'a, tuf::Result<Reader<'a>>> {
        let meta_path = meta_path.clone();
        async move {
            trace!(%meta_path, %version, "fetching TUF metadata over http");
            let url = self.url(METADATA_PREFIX, &meta_path.components::<Pouf1>(version))?;
            match self.fetch(url).await {
                Ok(bytes) => Ok(Box::new(Cursor::new(bytes)) as Reader<'a>),
                Err(err) if is_not_found(&err) => {
                    trace!(%meta_path, %version, "TUF metadata not found");
                    Err(tuf::Error::MetadataNotFound {
                        path: meta_path,
                        version,
                    })
                }
                Err(err) => {
                    debug!(%meta_path, %version, error = format!("{err:#}"), "TUF metadata fetch failed");
                    Err(tuf::Error::Opaque(format!("{err:#}")))
                }
            }
        }
        .boxed()
    }

    fn fetch_target<'a>(
        &'a self,
        target_path: &TargetPath,
    ) -> BoxFuture<'a, tuf::Result<Reader<'a>>> {
        let target_path = target_path.clone();
        async move {
            let url = self.url(TARGETS_PREFIX, &target_path.components())?;
            trace!(%target_path, %url, "fetching TUF target over http");
            match self.fetch(url).await {
                Ok(bytes) => Ok(Box::new(Cursor::new(bytes)) as Reader<'a>),
                Err(err) if is_not_found(&err) => {
                    trace!(%target_path, "TUF target not found");
                    Err(tuf::Error::TargetNotFound(target_path))
                }
                Err(err) => {
                    debug!(%target_path, error = format!("{err:#}"), "TUF target fetch failed");
                    Err(tuf::Error::Opaque(format!("{err:#}")))
                }
            }
        }
        .boxed()
    }
}

enum Remote {
    FileSystem(FileSystemRepository<Pouf1>),
    Http(HttpRepository),
}

impl Remote {
    fn from_location(location: &str, config: &TufConfig, process: &Process) -> Result<Self> {
        if utils::is_directory(location) {
            debug!(
                path = location,
                "using filesystem TUF remote from directory path"
            );
            return Ok(Self::FileSystem(FileSystemRepository::new(location)));
        }

        let url = utils::parse_url(location)?;
        match url.scheme() {
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| anyhow!("invalid TUF file url '{url}'"))?;
                debug!(path = %path.display(), "using filesystem TUF remote from file url");
                Ok(Self::FileSystem(FileSystemRepository::new(path)))
            }
            "http" | "https" => {
                let options = DownloadOptions::try_from(process)?;
                debug!(%url, "using http TUF remote");
                Ok(Self::Http(HttpRepository::new(
                    url,
                    options,
                    config.home.join(TMP_DIR),
                )))
            }
            scheme => bail!("unsupported TUF repository scheme '{scheme}' in '{url}'"),
        }
    }
}

impl RepositoryProvider<Pouf1> for Remote {
    fn fetch_metadata<'a>(
        &'a self,
        meta_path: &MetadataPath,
        version: MetadataVersion,
    ) -> BoxFuture<'a, tuf::Result<Reader<'a>>> {
        match self {
            Self::FileSystem(repo) => repo.fetch_metadata(meta_path, version),
            Self::Http(repo) => repo.fetch_metadata(meta_path, version),
        }
    }

    fn fetch_target<'a>(
        &'a self,
        target_path: &TargetPath,
    ) -> BoxFuture<'a, tuf::Result<Reader<'a>>> {
        match self {
            Self::FileSystem(repo) => repo.fetch_target(target_path),
            Self::Http(repo) => repo.fetch_target(target_path),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verification {
    Verified,
    Skipped,
}

pub(crate) struct TufRepository {
    config: TufConfig,
    client: Client<Pouf1, FileSystemRepository<Pouf1>, Remote>,
}

impl TufRepository {
    pub(crate) async fn open(
        config: &TufConfig,
        location: &str,
        process: &Process,
    ) -> Result<Self> {
        debug!(
            location,
            home = %config.home.display(),
            mode = %config.mode,
            ignore_failures = config.ignore_failures,
            ignore_expiry_after = ?config.ignore_expiry_after,
            "opening TUF repository"
        );
        utils::ensure_dir_exists("tuf home", &config.home)?;
        let local = FileSystemRepository::new(&config.home);
        let remote = Remote::from_location(location, config, process)?;

        let client = match &config.root {
            Some(path) => {
                debug!(path = %path.display(), "using trusted TUF root from RUSTUP_TUF_ROOT");
                let bytes = utils::read_file("tuf root", path)?.into_bytes();
                trace!(len = bytes.len(), "read trusted TUF root");
                let root = RawSignedMetadata::new(bytes);
                Client::with_trusted_root(Config::default(), &root, local, remote).await
            }
            None => {
                debug!(home = %config.home.display(), "using trusted TUF root from local cache");
                Client::with_trusted_local(Config::default(), local, remote).await
            }
        }
        .with_context(|| format!("error loading TUF repository from '{location}'"))?;

        let repo = Self {
            config: config.clone(),
            client,
        };
        repo.trace_database("loaded TUF trust database");
        Ok(repo)
    }

    pub(crate) async fn verify(&mut self) -> Result<Verification> {
        if self.config.mode == TufMode::Off {
            debug!("TUF mode is off, skipping metadata update");
            return Ok(Verification::Skipped);
        }
        debug!("updating TUF metadata from remote");
        match self.client.update().await {
            Ok(updated) => {
                debug!(updated, "TUF metadata update succeeded");
                self.trace_database("TUF trust database after update");
                Ok(Verification::Verified)
            }
            Err(err) => {
                debug!(error = %err, "TUF metadata update failed");
                self.tolerate(err.into())
            }
        }
    }

    pub(crate) async fn fetch_target(&mut self, target: &str) -> Result<(Vec<u8>, Verification)> {
        let path = TargetPath::new(target)?;
        debug!(target, mode = %self.config.mode, "fetching TUF target");
        if self.config.mode == TufMode::Off {
            debug!(target, "TUF mode is off, reading target unverified");
            return Ok((self.read_unverified(&path).await?, Verification::Skipped));
        }
        match self.read_verified(&path).await {
            Ok(bytes) => {
                debug!(target, len = bytes.len(), "TUF target verified");
                Ok((bytes, Verification::Verified))
            }
            Err(err) => {
                debug!(target, error = %err, "TUF target verification failed");
                let verification = self.tolerate(err.into())?;
                debug!(
                    target,
                    "reading TUF target unverified after tolerated failure"
                );
                Ok((self.read_unverified(&path).await?, verification))
            }
        }
    }

    async fn read_verified(&mut self, path: &TargetPath) -> tuf::Result<Vec<u8>> {
        trace!(target = %path, "reading TUF target through client");
        let mut reader = self.client.fetch_target(path).await?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        trace!(target = %path, len = bytes.len(), "read verified TUF target");
        Ok(bytes)
    }

    async fn read_unverified(&self, path: &TargetPath) -> tuf::Result<Vec<u8>> {
        let mut candidates = Vec::new();
        let database = self.client.database();
        if database.trusted_root().consistent_snapshot()
            && let Some(description) = database
                .trusted_targets()
                .and_then(|targets| targets.targets().get(path))
        {
            for digest in description.hashes().values() {
                candidates.push(path.with_hash_prefix(digest)?);
            }
        }
        candidates.push(path.clone());
        trace!(target = %path, ?candidates, "reading TUF target without verification");

        let mut last_err = tuf::Error::TargetNotFound(path.clone());
        for candidate in &candidates {
            match self.client.remote_repo().fetch_target(candidate).await {
                Ok(mut reader) => {
                    let mut bytes = Vec::new();
                    reader.read_to_end(&mut bytes).await?;
                    trace!(target = %path, %candidate, len = bytes.len(), "read unverified TUF target");
                    return Ok(bytes);
                }
                Err(err) => {
                    trace!(target = %path, %candidate, error = %err, "unverified TUF target candidate failed");
                    last_err = err;
                }
            }
        }
        Err(last_err)
    }

    fn tolerate(&self, err: anyhow::Error) -> Result<Verification> {
        let reason = match self.config.mode {
            TufMode::Off => Some("mode is off"),
            TufMode::Warn => Some("mode is warn"),
            TufMode::On if self.config.ignore_failures => Some("RUSTUP_TUF_IGNORE is set"),
            TufMode::On if self.expiry_ignored(&err) => {
                Some("expiry is after RUSTUP_TUF_IGNOREDATE")
            }
            TufMode::On => None,
        };
        match reason {
            Some(reason) => {
                debug!(reason, "tolerating TUF verification failure");
                warn!("TUF verification failed: {err:#}");
                Ok(Verification::Skipped)
            }
            None => {
                debug!(mode = %self.config.mode, "TUF verification failure is fatal");
                Err(err.context("TUF verification failed"))
            }
        }
    }

    fn expiry_ignored(&self, err: &anyhow::Error) -> bool {
        let Some(ignore_after) = self.config.ignore_expiry_after else {
            return false;
        };
        err.chain().any(|e| match e.downcast_ref::<tuf::Error>() {
            Some(tuf::Error::ExpiredMetadata {
                path, expiration, ..
            }) => {
                let ignored = *expiration > ignore_after;
                trace!(%path, %expiration, %ignore_after, ignored, "checking TUF expiry against ignore date");
                ignored
            }
            _ => false,
        })
    }

    fn trace_database(&self, message: &str) {
        let database: &Database<Pouf1> = self.client.database();
        let root = database.trusted_root();
        trace!(
            root_version = root.version(),
            root_expires = %root.expires(),
            consistent_snapshot = root.consistent_snapshot(),
            timestamp_version = database.trusted_timestamp().map(|m| m.version()),
            timestamp_expires = database.trusted_timestamp().map(|m| m.expires().to_string()),
            snapshot_version = database.trusted_snapshot().map(|m| m.version()),
            snapshot_expires = database.trusted_snapshot().map(|m| m.expires().to_string()),
            targets_version = database.trusted_targets().map(|m| m.version()),
            targets_expires = database.trusted_targets().map(|m| m.expires().to_string()),
            targets_count = database.trusted_targets().map(|m| m.targets().len()),
            delegations = database.trusted_delegations().len(),
            "{message}"
        );
    }
}

impl Drop for HttpRepository {
    fn drop(&mut self) {
        self.tmp_cx.clean();
    }
}
