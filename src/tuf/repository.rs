use std::{fmt, fs, path::Path};

use anyhow::{Context, anyhow, bail};
use bytes::Bytes;
use futures_util::{FutureExt, future::BoxFuture, stream};
use tracing::{debug, info, trace, warn};
use tuffet::{
    Updater,
    store::FilesystemStore,
    transport::{ByteStream, FilesystemTransport, Transport, TransportError},
};
use url::Url;

use crate::{
    download::DownloadOptions,
    errors::RustupError,
    tuf::{
        TufConfig,
        consts::{METADATA_PREFIX, ROOT, TARGETS_PREFIX},
    },
    utils,
};

/// A TUF client over the dist repository: a local metadata cache under
/// [`TufConfig::home`] kept in sync with the remote at [`TufConfig::server`].
pub(crate) struct TufRepository {
    ignore_failures: bool,
    updater: Updater<Remote, FilesystemStore>,
}

impl TufRepository {
    /// Opens the repository at [`TufConfig::server`]. The remote's own
    /// metadata and target downloads use `options` and are never themselves
    /// TUF-verified, which is what keeps this from recursing.
    #[tracing::instrument(level = "trace", err(level = "trace"), skip_all)]
    pub(crate) async fn open(config: &TufConfig, options: DownloadOptions) -> anyhow::Result<Self> {
        let location = config.server.as_str();

        info!("syncing TUF database from {}", location);

        if config.ignore_failures {
            warn!("RUSTUP_TUF_IGNORE is set, TUF verification failures will be ignored");
        }

        debug!(
            location,
            home = %config.home.display(),
            ignore_failures = config.ignore_failures,
            "opening TUF repository"
        );
        utils::ensure_dir_exists("tuf home", &config.home)?;
        let local = FilesystemStore::new(&config.home)
            .with_context(|| format!("error opening TUF home '{}'", config.home.display()))?;
        let remote = Remote::from_location(location, options)?;

        let bytes = match &config.root {
            Some(path) => {
                debug!(path = %path.display(), "using trusted TUF root from RUSTUP_TUF_ROOT");
                utils::read_file("tuf root", path)?.into_bytes()
            }
            None => {
                debug!("using trusted TUF root shipped with rustup");
                ROOT.to_vec()
            }
        };
        trace!(len = bytes.len(), "read trusted TUF root");
        let updater = Updater::builder(remote, local)
            .initial_root(bytes)
            .build()
            .with_context(|| format!("error loading TUF repository from '{location}'"))?;

        Ok(Self {
            ignore_failures: config.ignore_failures,
            updater,
        })
    }

    /// Updates the metadata from the remote, verifying the chain of trust.
    #[tracing::instrument(level = "trace", err(level = "trace"), skip_all)]
    pub(crate) async fn verify(&mut self) -> anyhow::Result<()> {
        debug!("updating TUF metadata from remote");
        match self.updater.refresh().await {
            Ok(()) => {
                debug!("TUF metadata update succeeded");
                Ok(())
            }
            Err(err) => self.tolerate(err.into()),
        }
    }

    /// Reads the target named `target`, verified against the trusted metadata.
    ///
    /// Returns `None` when verification failed but `RUSTUP_TUF_IGNORE` says to
    /// carry on; the caller then downloads the file without TUF.
    #[tracing::instrument(level = "trace", err(level = "trace"), skip_all)]
    pub(crate) async fn fetch_target(&mut self, target: &str) -> anyhow::Result<Option<Vec<u8>>> {
        debug!(target, "fetching TUF target");
        match self.read_verified(target).await {
            Ok(bytes) => {
                debug!(target, len = bytes.len(), "TUF target verified");
                Ok(Some(bytes))
            }
            Err(err) => {
                self.tolerate(err)?;
                Ok(None)
            }
        }
    }

    async fn read_verified(&mut self, target: &str) -> anyhow::Result<Vec<u8>> {
        let info = self
            .updater
            .get_target_info(target)
            .await?
            .ok_or_else(|| anyhow!("target '{target}' is not listed in the TUF metadata"))?;
        Ok(self.updater.download_target(&info).await?)
    }

    fn tolerate(&self, err: anyhow::Error) -> anyhow::Result<()> {
        if !self.ignore_failures {
            return Err(err.context("TUF verification failed"));
        }
        warn!("TUF verification failed, ignored because RUSTUP_TUF_IGNORE is set: {err:#}");
        Ok(())
    }
}

/// The remote side of the repository, served either from a local directory
/// (for tests and mirrors on disk) or over HTTP.
enum Remote {
    FileSystem(FilesystemTransport),
    Http(HttpRepository),
}

impl Remote {
    fn file_system(dir: &Path) -> Self {
        Self::FileSystem(FilesystemTransport::new(
            dir.join(METADATA_PREFIX),
            dir.join(TARGETS_PREFIX),
        ))
    }

    fn from_location(location: &str, options: DownloadOptions) -> anyhow::Result<Self> {
        if utils::is_directory(location) {
            debug!(
                path = location,
                "using filesystem TUF remote from directory path"
            );
            return Ok(Self::file_system(Path::new(location)));
        }

        let url = utils::parse_url(location)?;
        Ok(match url.scheme() {
            "file" => {
                let path = url
                    .to_file_path()
                    .map_err(|_| anyhow!("invalid TUF file url '{url}'"))?;
                debug!(path = %path.display(), "using filesystem TUF remote from file url");
                Self::file_system(&path)
            }
            "http" | "https" => {
                debug!(%url, "using http TUF remote");
                Self::Http(HttpRepository::new(url, options))
            }
            scheme => bail!("unsupported TUF repository scheme '{scheme}' in '{url}'"),
        })
    }
}

impl Transport for Remote {
    async fn fetch_metadata(
        &self,
        name: &str,
        max_length: u64,
    ) -> Result<ByteStream, TransportError> {
        match self {
            Self::FileSystem(repo) => repo.fetch_metadata(name, max_length).await,
            Self::Http(repo) => repo.fetch_metadata(name, max_length).await,
        }
    }

    async fn fetch_target(
        &self,
        path: &str,
        max_length: u64,
    ) -> Result<ByteStream, TransportError> {
        match self {
            Self::FileSystem(repo) => repo.fetch_target(path, max_length).await,
            Self::Http(repo) => repo.fetch_target(path, max_length).await,
        }
    }
}

/// A remote reached through rustup's own downloader, so proxies, TLS backend
/// and timeouts behave exactly as for every other download.
struct HttpRepository {
    base: Url,
    options: DownloadOptions,
}

impl HttpRepository {
    fn new(base: Url, options: DownloadOptions) -> Self {
        trace!(%base, ?options, "created TUF http repository");
        Self { base, options }
    }

    fn url<'a>(
        &self,
        prefix: &str,
        components: impl IntoIterator<Item = &'a str> + fmt::Debug + Clone,
    ) -> Result<Url, TransportError> {
        let mut url = self.base.clone();
        {
            let mut segments = url.path_segments_mut().map_err(|_| {
                TransportError::other(format!("cannot be a base url: {}", self.base))
            })?;
            segments.pop_if_empty();
            segments.push(prefix);
            segments.extend(components.clone());
        }
        trace!(prefix, ?components, %url, "resolved TUF url");
        Ok(url)
    }

    fn fetch(&self, url: Url) -> BoxFuture<'_, anyhow::Result<Vec<u8>>> {
        async move {
            // Removed when dropped, whether or not the download succeeds.
            let file = tempfile::Builder::new()
                .prefix("rustup-tuf")
                .tempfile()
                .context("error creating temp file for TUF download")?;
            let path = file.path();
            debug!(%url, path = %path.display(), "fetching TUF file");
            self.options.start(&url, path, None).download().await?;
            let bytes = fs::read(path)
                .with_context(|| format!("error reading TUF file '{}'", path.display()))?;
            trace!(%url, len = bytes.len(), "fetched TUF file");
            Ok(bytes)
        }
        .boxed()
    }
}

impl Transport for HttpRepository {
    async fn fetch_metadata(
        &self,
        name: &str,
        _max_length: u64,
    ) -> Result<ByteStream, TransportError> {
        trace!(name, "fetching TUF metadata over http");
        let url = self.url(METADATA_PREFIX, [name])?;
        match self.fetch(url).await {
            Ok(bytes) => Ok(byte_stream(bytes)),
            Err(err) if is_not_found(&err) => {
                trace!(name, "TUF metadata not found");
                Err(TransportError::NotFound)
            }
            Err(err) => {
                debug!(
                    name,
                    error = format!("{err:#}"),
                    "TUF metadata fetch failed"
                );
                Err(TransportError::other(err))
            }
        }
    }

    async fn fetch_target(
        &self,
        path: &str,
        _max_length: u64,
    ) -> Result<ByteStream, TransportError> {
        let url = self.url(TARGETS_PREFIX, path.split('/'))?;
        trace!(target_path = path, %url, "fetching TUF target over http");
        match self.fetch(url).await {
            Ok(bytes) => Ok(byte_stream(bytes)),
            Err(err) if is_not_found(&err) => {
                trace!(target_path = path, "TUF target not found");
                Err(TransportError::NotFound)
            }
            Err(err) => {
                debug!(
                    target_path = path,
                    error = format!("{err:#}"),
                    "TUF target fetch failed"
                );
                Err(TransportError::other(err))
            }
        }
    }
}

fn byte_stream(bytes: Vec<u8>) -> ByteStream {
    Box::pin(stream::iter([Ok(Bytes::from(bytes))]))
}

fn is_not_found(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<RustupError>(),
        Some(RustupError::DownloadNotExists { .. })
    )
}
