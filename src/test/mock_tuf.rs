//! A TUF repository signed over the mock dist and self-update servers.

use std::{
    array, fs,
    path::{Path, PathBuf},
};

use tempfile::TempDir;
use tuffet::{
    crypto::{HashAlgorithm, PrivateKey, Signer},
    jiff::{SignedDuration, Timestamp as Time},
    metadata::{
        MetaFile, Metadata, Role, RoleType, Root, SignatureScheme, Snapshot, TargetFile, Targets,
        Timestamp,
    },
    pouf::{Json, Pouf},
};

/// A dist server that cannot be reached, so a manifest can only arrive through TUF.
const UNREACHABLE_DIST_SERVER: &str = "https://dist.invalid";
/// Likewise for rustup's own release file and binaries.
const UNREACHABLE_UPDATE_ROOT: &str = "https://dist.invalid/rustup";

/// A TUF repository over a mock dist server, and optionally a mock self-update
/// server, signed with throwaway keys.
///
/// Manifests are published in the `channels/` layout that rustup looks them up
/// in with TUF enabled, and self-update files under `rustup/`. Call
/// [`MockTufServer::publish`] again after changing the mocks; each publication
/// is a fresh repository and a fresh metadata cache.
pub struct MockTufServer {
    dist: PathBuf,
    self_dist: Option<PathBuf>,
    keys: [PrivateKey; 4],
    generation: usize,
    repo: String,
    root: String,
    home: String,
    _dir: TempDir,
}

impl MockTufServer {
    pub async fn new(dist: &Path, self_dist: Option<&Path>) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("rustup-tuf-mock")
            .tempdir()
            .unwrap();
        let keys = array::from_fn(|_| PrivateKey::generate(&SignatureScheme::Ed25519).unwrap());
        let mut server = Self {
            dist: dist.to_owned(),
            self_dist: self_dist.map(Path::to_owned),
            keys,
            generation: 0,
            repo: String::new(),
            root: String::new(),
            home: String::new(),
            _dir: dir,
        };
        server.publish().await;
        server
    }

    /// Signs the mocks' current contents into a new repository.
    pub async fn publish(&mut self) {
        self.generation += 1;
        let repo = self._dir.path().join(format!("repo-{}", self.generation));
        let home = self._dir.path().join(format!("home-{}", self.generation));
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&home).unwrap();

        let metadata = repo.join("metadata");
        let targets_dir = repo.join("targets");
        fs::create_dir_all(&metadata).unwrap();

        let expires = Time::now() + SignedDuration::from_hours(365 * 24);
        let [root_key, targets_key, snapshot_key, timestamp_key] = &self.keys;

        let mut root = Root::new(expires);
        for (role, key) in [
            (RoleType::Root, root_key),
            (RoleType::Targets, targets_key),
            (RoleType::Snapshot, snapshot_key),
            (RoleType::Timestamp, timestamp_key),
        ] {
            let public = key.public_key();
            root.add_key(role, Json::key_id(public).unwrap(), public.clone());
        }
        let root = signed(root, root_key);
        fs::write(metadata.join("1.root.json"), &root).unwrap();
        fs::write(metadata.join("root.json"), &root).unwrap();

        let mut targets = Targets::new(expires);
        for (target, bytes) in self.targets() {
            let file = TargetFile::from_bytes(&bytes, &[HashAlgorithm::Sha256]);
            let digest = file.hashes.preferred().unwrap().to_string();
            let (dir, name) = target.rsplit_once('/').unwrap();
            let dir = targets_dir.join(dir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join(format!("{digest}.{name}")), &bytes).unwrap();
            targets.targets.insert(target, file);
        }
        let targets = signed(targets, targets_key);
        fs::write(metadata.join("1.targets.json"), &targets).unwrap();
        fs::write(metadata.join("targets.json"), &targets).unwrap();

        let mut snapshot = Snapshot::new(expires);
        snapshot.meta.insert(
            "targets.json".to_owned(),
            MetaFile::from_bytes(1, &targets, &[HashAlgorithm::Sha256]),
        );
        let snapshot = signed(snapshot, snapshot_key);
        fs::write(metadata.join("1.snapshot.json"), &snapshot).unwrap();
        fs::write(metadata.join("snapshot.json"), &snapshot).unwrap();

        let mut timestamp = Timestamp::new(expires);
        timestamp.meta.insert(
            "snapshot.json".to_owned(),
            MetaFile::from_bytes(1, &snapshot, &[HashAlgorithm::Sha256]),
        );
        let timestamp = signed(timestamp, timestamp_key);
        fs::write(metadata.join("timestamp.json"), &timestamp).unwrap();

        self.root = metadata.join("1.root.json").to_string_lossy().into_owned();
        self.repo = repo.to_string_lossy().into_owned();
        self.home = home.to_string_lossy().into_owned();
    }

    /// Environment for a rustup command that must get its manifests, release
    /// file and binaries through this repository, verified in `mode`.
    pub fn env(&self, mode: &'static str) -> [(&str, &str); 6] {
        let [enable, server, root, home] = self.settings(mode);
        [
            enable,
            server,
            root,
            home,
            ("RUSTUP_DIST_SERVER", UNREACHABLE_DIST_SERVER),
            ("RUSTUP_UPDATE_ROOT", UNREACHABLE_UPDATE_ROOT),
        ]
    }

    /// Just the TUF settings, leaving the mock servers reachable.
    pub fn settings(&self, mode: &'static str) -> [(&str, &str); 4] {
        [
            ("RUSTUP_TUF_ENABLE", mode),
            ("RUSTUP_TUF_SERVER", &self.repo),
            ("RUSTUP_TUF_ROOT", &self.root),
            ("RUSTUP_TUF_HOME", &self.home),
        ]
    }

    /// Alters the published copy of `target` after signing, so it no longer
    /// matches the metadata.
    pub fn tamper(&self, target: &str) {
        let (dir, name) = target.rsplit_once('/').unwrap();
        let dir = Path::new(&self.repo).join("targets").join(dir);
        let path = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|file| file.to_string_lossy().ends_with(&format!(".{name}")))
            })
            .unwrap_or_else(|| panic!("no published copy of {target} in {}", dir.display()));
        let mut bytes = fs::read(&path).unwrap();
        bytes.extend_from_slice(b"\n# tampered after signing\n");
        fs::write(&path, bytes).unwrap();
    }

    /// Every target to publish, named as rustup will ask for it.
    fn targets(&self) -> Vec<(String, Vec<u8>)> {
        let mut targets = Vec::new();
        let dist = self.dist.join("dist");
        for entry in fs::read_dir(&dist).unwrap() {
            let path = entry.unwrap().path();
            let file = path.file_name().unwrap().to_string_lossy().into_owned();
            if path.is_dir() {
                // `<date>/channel-rust-<channel>.toml`
                let Some((year, month_day)) = file.split_once('-') else {
                    continue;
                };
                for entry in fs::read_dir(&path).unwrap() {
                    let path = entry.unwrap().path();
                    let file = path.file_name().unwrap().to_string_lossy().into_owned();
                    if let Some(channel) = manifest_channel(&file) {
                        targets.push((
                            format!("channels/archive/{year}/{month_day}/{channel}.toml"),
                            fs::read(&path).unwrap(),
                        ));
                    }
                }
            } else if let Some(channel) = manifest_channel(&file) {
                targets.push((
                    format!("channels/current/{channel}.toml"),
                    fs::read(&path).unwrap(),
                ));
            }
        }

        if let Some(self_dist) = &self.self_dist {
            targets.push((
                "rustup/release-stable.toml".to_owned(),
                fs::read(self_dist.join("release-stable.toml")).unwrap(),
            ));
            // `archive/<version>/<target>/rustup-init` is published under
            // `rustup/archive/`, as the reference repository does.
            for version in fs::read_dir(self_dist.join("archive")).unwrap() {
                let version = version.unwrap().path();
                let release = version.file_name().unwrap().to_string_lossy().into_owned();
                targets.extend(rustup_init_targets(
                    &version,
                    &format!("rustup/archive/{release}"),
                ));
            }
            // `dist/<target>/rustup-init`, the latest release that the version
            // in `release-stable.toml` refers to, under `rustup/dist/`.
            targets.extend(rustup_init_targets(&self_dist.join("dist"), "rustup/dist"));
        }
        targets
    }
}

fn signed(role: impl Role, key: &PrivateKey) -> Vec<u8> {
    let mut metadata = Metadata::<_, Json>::new(role).unwrap();
    metadata.sign(key).unwrap();
    metadata.to_bytes().unwrap()
}

/// Every `rustup-init` binary under `dir/<target>/`, named `<prefix>/<target>/<file>`.
fn rustup_init_targets(dir: &Path, prefix: &str) -> Vec<(String, Vec<u8>)> {
    let mut targets = Vec::new();
    for triple in fs::read_dir(dir).unwrap() {
        let triple = triple.unwrap().path();
        let name = triple.file_name().unwrap().to_string_lossy().into_owned();
        for bin in fs::read_dir(&triple).unwrap() {
            let bin = bin.unwrap().path();
            let file = bin.file_name().unwrap().to_string_lossy().into_owned();
            if !file.starts_with("rustup-init") || file.contains("tmp") {
                continue;
            }
            targets.push((format!("{prefix}/{name}/{file}"), fs::read(&bin).unwrap()));
        }
    }
    targets
}

/// The channel of a v2 manifest file name, `channel-rust-<channel>.toml`.
fn manifest_channel(file: &str) -> Option<&str> {
    file.strip_prefix("channel-rust-")?.strip_suffix(".toml")
}
