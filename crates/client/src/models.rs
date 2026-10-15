//! Downloading and verifying Whisper models.
//!
//! Downloads run in the background without a progress bar, are written to a
//! temporary file, verified against the SHA-256 the server announced and only
//! then moved into place, so a half-downloaded model is never used.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};
use verbarelay_protocol::models::ModelSpec;

/// Directory with the downloaded models.
#[derive(Clone)]
pub struct ModelStore {
    dir: PathBuf,
    in_progress: Arc<Mutex<HashSet<String>>>,
}

impl ModelStore {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            in_progress: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn path_of(&self, spec: &ModelSpec) -> PathBuf {
        self.dir.join(spec.file_name())
    }

    /// Path of a model by name (for the peer's own model choice).
    pub fn path_of_name(&self, name: &str) -> Option<PathBuf> {
        let spec = verbarelay_protocol::models::find(name)?;
        let path = self.path_of(&spec);
        path.exists().then_some(path)
    }

    pub fn has(&self, spec: &ModelSpec) -> bool {
        self.path_of(spec).metadata().is_ok_and(|m| m.len() == spec.size)
    }

    /// Names of the complete models on disk (catalog models and the given extra specs).
    pub fn available(&self, extra: &[ModelSpec]) -> Vec<String> {
        let mut names: Vec<String> = verbarelay_protocol::models::catalog()
            .iter()
            .chain(extra.iter())
            .filter(|m| self.has(m))
            .map(|m| m.name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Marks a model as being downloaded. Returns false if a download already runs.
    pub fn claim(&self, name: &str) -> bool {
        self.in_progress.lock().unwrap().insert(name.to_string())
    }

    /// Downloads a model unless it is already present. Returns its path.
    pub async fn ensure(&self, client: &reqwest::Client, spec: &ModelSpec) -> anyhow::Result<PathBuf> {
        if self.has(spec) {
            return Ok(self.path_of(spec));
        }
        if !self.claim(&spec.name) {
            bail!("model '{}' is already being downloaded", spec.name);
        }
        self.download_claimed(client, spec).await
    }

    /// Downloads a model previously claimed with [`ModelStore::claim`].
    pub async fn download_claimed(&self, client: &reqwest::Client, spec: &ModelSpec) -> anyhow::Result<PathBuf> {
        let target = self.path_of(spec);
        let result = if spec.url.starts_with("https://") {
            download_verified(client, &spec.url, &spec.sha256, &target).await
        } else {
            Err(anyhow::anyhow!(
                "refusing to download model '{}' over an insecure connection",
                spec.name
            ))
        };
        self.in_progress.lock().unwrap().remove(&spec.name);
        result.map(|()| target)
    }
}

/// Downloads `url` to `target`, verifying the SHA-256 before moving it into place.
pub async fn download_verified(client: &reqwest::Client, url: &str, sha256: &str, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let partial = target.with_extension("partial");
    info!(%url, "download started");
    let response = client
        .get(url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;
    let mut file = tokio::fs::File::create(&partial).await?;
    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    let mut total = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
        total += chunk.len() as u64;
    }
    file.flush().await?;
    file.sync_all().await?;
    drop(file);

    let digest: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    if !digest.eq_ignore_ascii_case(sha256) {
        let _ = tokio::fs::remove_file(&partial).await;
        warn!(%url, expected = %sha256, actual = %digest, "checksum mismatch");
        bail!("the download of {url} is corrupt (checksum mismatch)");
    }
    tokio::fs::rename(&partial, target).await?;
    info!(%url, bytes = total, "download finished and verified");
    Ok(())
}
