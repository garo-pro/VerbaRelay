//! Release metadata on GitHub, signature verification and archive extraction.
//!
//! Release layout (produced by `.github/workflows/release.yml`):
//!
//! * stable releases are tagged `vX.Y.Z`
//! * development builds live in one rolling pre-release tagged `dev`, with a
//!   `dev-build.json` asset naming the commit
//! * every archive has a minisign signature next to it (`<asset>.minisig`)

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::io::AsyncWriteExt;
use tracing::info;

/// Facts compiled into this build.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub version: semver::Version,
    pub commit: String,
    /// "stable" or "dev"
    pub channel: String,
    /// GitHub "owner/repo", if updates are configured.
    pub repo: Option<String>,
    /// minisign public key, if updates are configured.
    pub public_key: Option<String>,
}

impl BuildInfo {
    pub fn current() -> Self {
        let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        Self {
            version: semver::Version::parse(env!("CARGO_PKG_VERSION")).expect("valid package version"),
            commit: env!("VERBARELAY_BUILD_COMMIT").to_string(),
            channel: env!("VERBARELAY_BUILD_CHANNEL").to_string(),
            repo: non_empty(env!("VERBARELAY_BUILD_REPO")),
            public_key: non_empty(env!("VERBARELAY_BUILD_PUBKEY")),
        }
    }

    pub fn is_dev(&self) -> bool {
        self.channel == "dev"
    }

    /// The release label of this build: "1.2.3" or "dev".
    pub fn label(&self) -> String {
        if self.is_dev() {
            "dev".into()
        } else {
            self.version.to_string()
        }
    }

    pub fn updates_configured(&self) -> bool {
        self.repo.is_some() && self.public_key.is_some()
    }

    pub fn describe(&self) -> String {
        let short: String = self.commit.chars().take(8).collect();
        if self.is_dev() {
            format!("{} (development build {short})", self.version)
        } else {
            self.version.to_string()
        }
    }
}

/// Platform part of asset names.
pub fn target_label() -> &'static str {
    if cfg!(windows) {
        "windows-x86_64"
    } else if cfg!(target_os = "macos") {
        "macos-arm64"
    } else if cfg!(target_arch = "aarch64") {
        "linux-aarch64"
    } else {
        "linux-x86_64"
    }
}

pub fn client_asset_name(label: &str) -> String {
    format!("verbarelay-client-{label}-{}.zip", target_label())
}

pub fn cuda_asset_name(label: &str) -> String {
    format!("verbarelay-cuda-{label}-{}.zip", target_label())
}

pub const DEV_TAG: &str = "dev";
pub const DEV_MANIFEST: &str = "dev-build.json";

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// Contents of `dev-build.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct DevManifest {
    pub commit: String,
}

impl Release {
    pub fn asset(&self, name: &str) -> Option<&Asset> {
        self.assets.iter().find(|a| a.name == name)
    }

    /// The version of a stable release tag ("v1.2.3").
    pub fn stable_version(&self) -> Option<semver::Version> {
        if self.prerelease || self.draft {
            return None;
        }
        semver::Version::parse(self.tag_name.trim_start_matches('v'))
            .ok()
            .filter(|v| v.pre.is_empty())
    }
}

/// Lists the newest releases of the repository.
pub async fn list_releases(client: &reqwest::Client, repo: &str) -> anyhow::Result<Vec<Release>> {
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
    let releases = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("querying {url}"))?
        .json()
        .await?;
    Ok(releases)
}

/// Finds the release that matches this build (for downloading the CUDA package).
pub async fn release_for_build(client: &reqwest::Client, info: &BuildInfo) -> anyhow::Result<Release> {
    let repo = info
        .repo
        .as_deref()
        .context("this build has no release repository configured")?;
    let wanted = if info.is_dev() {
        DEV_TAG.to_string()
    } else {
        format!("v{}", info.version)
    };
    list_releases(client, repo)
        .await?
        .into_iter()
        .find(|r| r.tag_name == wanted)
        .with_context(|| format!("release {wanted} not found in {repo}"))
}

pub async fn fetch_dev_manifest(client: &reqwest::Client, release: &Release) -> anyhow::Result<DevManifest> {
    let asset = release
        .asset(DEV_MANIFEST)
        .context("the dev release has no build manifest")?;
    Ok(client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?)
}

/// Downloads an asset to `target` and verifies its minisign signature on the fly.
pub async fn download_signed(
    client: &reqwest::Client,
    release: &Release,
    name: &str,
    public_key: &str,
    target: &Path,
) -> anyhow::Result<()> {
    let asset = release
        .asset(name)
        .with_context(|| format!("release {} has no asset {name}", release.tag_name))?;
    let sig_asset = release
        .asset(&format!("{name}.minisig"))
        .with_context(|| format!("{name} is not signed"))?;
    let signature_text = client
        .get(&sig_asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let signature = minisign_verify::Signature::decode(&signature_text).context("invalid signature file")?;
    let key = minisign_verify::PublicKey::from_base64(public_key).context("invalid update public key")?;
    let mut verifier = key
        .verify_stream(&signature)
        .context("signature does not match the update key")?;

    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    info!(asset = %name, release = %release.tag_name, "downloading");
    let response = client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()?;
    let mut file = tokio::fs::File::create(target).await?;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("download interrupted")?;
        verifier.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    drop(file);
    if let Err(e) = verifier.finalize() {
        let _ = tokio::fs::remove_file(target).await;
        bail!("signature check of {name} failed: {e}");
    }
    info!(asset = %name, "signature verified");
    Ok(())
}

/// Extracts a zip archive into `dest`, rejecting paths that leave it.
/// Returns the relative paths of the extracted files.
pub fn extract_zip(archive: &Path, dest: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file).context("opening update archive")?;
    let mut files = Vec::new();
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let Some(relative) = entry.enclosed_name() else {
            bail!("archive entry with an unsafe path: {}", entry.name());
        };
        if relative.components().any(|c| !matches!(c, Component::Normal(_))) {
            bail!("archive entry with an unsafe path: {}", entry.name());
        }
        let out = dest.join(&relative);
        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        std::fs::write(&out, &data)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o777))?;
        }
        files.push(relative);
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn stable_versions() {
        let r = Release {
            tag_name: "v1.2.3".into(),
            prerelease: false,
            draft: false,
            assets: vec![],
        };
        assert_eq!(r.stable_version(), Some(semver::Version::new(1, 2, 3)));
        let dev = Release {
            tag_name: "dev".into(),
            prerelease: true,
            draft: false,
            assets: vec![],
        };
        assert_eq!(dev.stable_version(), None);
    }

    #[test]
    fn extracts_and_rejects_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&good).unwrap());
            let opts = zip::write::SimpleFileOptions::default();
            zip.start_file("bin/app", opts).unwrap();
            zip.write_all(b"hello").unwrap();
            zip.finish().unwrap();
        }
        let out = dir.path().join("out");
        let files = extract_zip(&good, &out).unwrap();
        assert_eq!(files, vec![PathBuf::from("bin/app")]);
        assert_eq!(std::fs::read(out.join("bin/app")).unwrap(), b"hello");

        let evil = dir.path().join("evil.zip");
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&evil).unwrap());
            zip.start_file("../escape", zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"x").unwrap();
            zip.finish().unwrap();
        }
        assert!(extract_zip(&evil, &out).is_err());
        assert!(!dir.path().join("escape").exists());
    }
}
