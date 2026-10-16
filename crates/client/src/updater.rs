//! Silent automatic updates.
//!
//! Inspired by ship-shape (github.com/trypsynth/ship-shape, MIT): stable releases
//! are compared by semantic version, development builds by commit hash, and every
//! download is verified with minisign before it is used. Unlike ship-shape this
//! updater has no user interface at all and also runs on Linux:
//!
//! 1. check GitHub releases every N minutes (the user setting) or when asked from the tray,
//! 2. download and verify the archive in the background (no progress bar),
//! 3. wait until no transcription is running,
//! 4. swap the files and restart.
//!
//! Running executables cannot be overwritten on Windows but can be renamed, so
//! every replaced file is first moved aside as `*.old` and cleaned up after the
//! restart.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, bail};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::config::UpdateSettings;
use crate::ipc::UpdateCheck;
use crate::release::{self, BuildInfo, Release};

/// An update that is downloaded, verified and extracted, ready to be applied.
#[derive(Debug, Clone)]
pub struct StagedUpdate {
    pub description: String,
    pub staging_dir: PathBuf,
    pub files: Vec<PathBuf>,
}

/// What the check found.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub release: Release,
    pub label: String,
    pub description: String,
}

/// Decides whether a release is newer than this build.
pub fn pick_candidate(
    info: &BuildInfo,
    settings: &UpdateSettings,
    releases: &[Release],
    dev_commit: Option<&str>,
) -> Option<Candidate> {
    if settings.include_dev_builds
        && let (Some(dev), Some(commit)) = (releases.iter().find(|r| r.tag_name == release::DEV_TAG), dev_commit)
    {
        if commit != info.commit {
            let short: String = commit.chars().take(8).collect();
            return Some(Candidate {
                release: dev.clone(),
                label: "dev".into(),
                description: format!("development build {short}"),
            });
        }
        return None;
    }
    let newest = releases
        .iter()
        .filter_map(|r| r.stable_version().map(|v| (v, r)))
        .max_by(|a, b| a.0.cmp(&b.0))?;
    // A development build switches back to stable once dev builds are turned off.
    let newer = newest.0 > info.version || (info.is_dev() && !settings.include_dev_builds && newest.0 >= info.version);
    newer.then(|| Candidate {
        release: newest.1.clone(),
        label: newest.0.to_string(),
        description: format!("version {}", newest.0),
    })
}

/// Checks for an update. Returns `None` when this build is current.
pub async fn check(
    client: &reqwest::Client,
    info: &BuildInfo,
    settings: &UpdateSettings,
) -> anyhow::Result<Option<Candidate>> {
    let repo = info
        .repo
        .as_deref()
        .context("updates are not configured for this build")?;
    let releases = release::list_releases(client, repo).await?;
    let dev_commit = if settings.include_dev_builds {
        match releases.iter().find(|r| r.tag_name == release::DEV_TAG) {
            Some(dev) => Some(release::fetch_dev_manifest(client, dev).await?.commit),
            None => None,
        }
    } else {
        None
    };
    Ok(pick_candidate(info, settings, &releases, dev_commit.as_deref()))
}

/// Downloads and extracts an update (plus the CUDA package if it is installed).
pub async fn stage(
    client: &reqwest::Client,
    info: &BuildInfo,
    candidate: &Candidate,
    install_dir: &Path,
    data_dir: &Path,
) -> anyhow::Result<StagedUpdate> {
    let public_key = info.public_key.as_deref().context("no update key")?;
    let downloads = data_dir.join("downloads");
    let staging_dir = data_dir.join("update-staging");
    let _ = tokio::fs::remove_dir_all(&staging_dir).await;

    let mut archives = vec![release::client_asset_name(&candidate.label)];
    if crate::packages::cuda_installed(install_dir) {
        archives.push(release::cuda_asset_name(&candidate.label));
    }
    let mut files = Vec::new();
    for name in archives {
        let archive = downloads.join(&name);
        release::download_signed(client, &candidate.release, &name, public_key, &archive).await?;
        let staging = staging_dir.clone();
        let archive_path = archive.clone();
        let extracted = tokio::task::spawn_blocking(move || release::extract_zip(&archive_path, &staging)).await??;
        files.extend(extracted);
        let _ = tokio::fs::remove_file(&archive).await;
    }
    if crate::packages::cuda_installed(install_dir) {
        let marker = PathBuf::from("cuda-package.txt");
        tokio::fs::write(staging_dir.join(&marker), format!("{}\n", candidate.label)).await?;
        files.push(marker);
    }
    let exe_name = std::env::current_exe()?
        .file_name()
        .map(PathBuf::from)
        .context("executable name")?;
    if !files.contains(&exe_name) {
        bail!("the update archive does not contain {}", exe_name.display());
    }
    Ok(StagedUpdate {
        description: candidate.description.clone(),
        staging_dir,
        files,
    })
}

/// Replaces `target` with `source`, moving a busy target aside first.
pub fn replace_file(source: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if target.exists() {
        let old = old_path(target);
        let _ = std::fs::remove_file(&old);
        std::fs::rename(target, &old).with_context(|| format!("moving {} aside", target.display()))?;
    }
    if std::fs::rename(source, target).is_err() {
        // Different file systems: copy instead.
        std::fs::copy(source, target).with_context(|| format!("installing {}", target.display()))?;
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

fn old_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".old");
    target.with_file_name(name)
}

/// Swaps the staged files into the installation directory.
pub fn apply(staged: &StagedUpdate, install_dir: &Path) -> anyhow::Result<()> {
    for relative in &staged.files {
        replace_file(&staged.staging_dir.join(relative), &install_dir.join(relative))?;
    }
    let _ = std::fs::remove_dir_all(&staged.staging_dir);
    info!(update = %staged.description, "update applied");
    Ok(())
}

/// Removes `*.old` files left by the previous update.
pub fn cleanup_old_files(install_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(install_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "old")
            && let Err(e) = std::fs::remove_file(&path)
        {
            debug!("could not remove {}: {e}", path.display());
        }
    }
}

/// Starts the new executable and returns; the caller exits afterwards.
/// Under systemd the service manager restarts us instead.
pub fn restart() -> anyhow::Result<()> {
    if std::env::var_os("INVOCATION_ID").is_some() {
        info!("running under systemd; exiting so the service restarts with the new version");
        return Ok(());
    }
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--after-update");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(DETACHED_PROCESS);
    }
    cmd.spawn().context("starting the updated client")?;
    Ok(())
}

/// Background loop: checks on the configured interval and on request, and
/// hands a staged update to the agent.
pub async fn run(
    mut settings: watch::Receiver<UpdateSettings>,
    install_dir: PathBuf,
    data_dir: PathBuf,
    staged_tx: mpsc::Sender<StagedUpdate>,
    mut manual: mpsc::Receiver<UpdateCheck>,
) {
    let info = BuildInfo::current();
    let client = if info.updates_configured() {
        crate::net::download_client()
            .inspect_err(|e| warn!("updater disabled: {e:#}"))
            .ok()
    } else {
        info!("automatic updates are not configured for this build");
        None
    };
    let Some(client) = client else {
        while let Some(reply) = manual.recv().await {
            let _ = reply.send("Automatic updates are not available in this build.".into());
        }
        return;
    };
    // First check shortly after start, then on the interval.
    let mut wait = Duration::from_secs(60);
    let mut staged: Option<String> = None;
    loop {
        let reply = tokio::select! {
            () = tokio::time::sleep(wait), if staged.is_none() => None,
            changed = settings.changed() => {
                if changed.is_err() { return; }
                // Settings changed: check soon with the new settings.
                wait = Duration::from_secs(5);
                continue;
            }
            request = manual.recv() => match request {
                Some(reply) => Some(reply),
                None => return,
            },
        };
        if let Some(description) = &staged {
            answer(reply, format!("{description} is already downloaded. {INSTALL_NOTE}"));
            continue;
        }
        let current = settings.borrow().clone();
        wait = Duration::from_secs(u64::from(current.check_interval_minutes) * 60);
        match check(&client, &info, &current).await {
            Ok(Some(candidate)) => {
                info!(update = %candidate.description, "update available");
                crate::tones::play(crate::tones::Tone::UpdateFound);
                let description = capitalized(&candidate.description);
                answer(
                    reply,
                    format!("{description} is available and is being downloaded. {INSTALL_NOTE}"),
                );
                match stage(&client, &info, &candidate, &install_dir, &data_dir).await {
                    Ok(update) => {
                        staged = Some(description);
                        let _ = staged_tx.send(update).await;
                    }
                    Err(e) => warn!("downloading the update failed: {e:#}"),
                }
            }
            Ok(None) => {
                debug!("no update available");
                answer(reply, format!("No update available. This is {}.", info.describe()));
            }
            Err(e) => {
                warn!("update check failed: {e:#}");
                answer(reply, format!("The update check failed: {e:#}"));
            }
        }
    }
}

const INSTALL_NOTE: &str = "It is installed automatically when no transcription is running.";

fn answer(reply: Option<oneshot::Sender<String>>, text: String) {
    if let Some(reply) = reply {
        let _ = reply.send(text);
    }
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(version: &str, channel: &str, commit: &str) -> BuildInfo {
        BuildInfo {
            version: semver::Version::parse(version).unwrap(),
            commit: commit.into(),
            channel: channel.into(),
            repo: Some("o/r".into()),
            public_key: Some("k".into()),
        }
    }

    fn rel(tag: &str, prerelease: bool) -> Release {
        Release {
            tag_name: tag.into(),
            prerelease,
            draft: false,
            assets: vec![],
        }
    }

    #[test]
    fn stable_updates() {
        let releases = vec![rel("v0.1.0", false), rel("v0.2.0", false), rel("dev", true)];
        let settings = UpdateSettings::default();
        let c = pick_candidate(&info("0.1.0", "stable", "a"), &settings, &releases, None).unwrap();
        assert_eq!(c.label, "0.2.0");
        assert!(pick_candidate(&info("0.2.0", "stable", "a"), &settings, &releases, None).is_none());
    }

    #[test]
    fn dev_updates_follow_commits() {
        let releases = vec![rel("v0.1.0", false), rel("dev", true)];
        let settings = UpdateSettings {
            check_interval_minutes: 360,
            include_dev_builds: true,
        };
        let c = pick_candidate(&info("0.1.0", "stable", "aaa"), &settings, &releases, Some("bbb")).unwrap();
        assert_eq!(c.label, "dev");
        assert!(pick_candidate(&info("0.1.0", "dev", "bbb"), &settings, &releases, Some("bbb")).is_none());
    }

    #[test]
    fn dev_build_returns_to_stable() {
        let releases = vec![rel("v0.1.0", false)];
        let c = pick_candidate(&info("0.1.0", "dev", "x"), &UpdateSettings::default(), &releases, None).unwrap();
        assert_eq!(c.label, "0.1.0");
    }

    #[test]
    fn replace_moves_old_file_aside() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("app");
        let source = dir.path().join("new");
        std::fs::write(&target, "old").unwrap();
        std::fs::write(&source, "new").unwrap();
        replace_file(&source, &target).unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert!(dir.path().join("app.old").exists());
        cleanup_old_files(dir.path());
        assert!(!dir.path().join("app.old").exists());
    }
}
