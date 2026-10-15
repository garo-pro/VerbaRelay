//! Optional native packages. Currently only the CUDA package: the CUDA backend
//! module plus the CUDA runtime and cuBLAS libraries, several hundred MB that only
//! NVIDIA users need. It is downloaded during setup and kept up to date by the
//! updater together with the client.

use std::path::Path;

use anyhow::Context;
use tracing::info;

use crate::release::{self, BuildInfo};

const CUDA_MARKER: &str = "cuda-package.txt";

/// Whether the CUDA package is installed next to the executable.
pub fn cuda_installed(install_dir: &Path) -> bool {
    install_dir.join(CUDA_MARKER).exists()
}

/// Downloads, verifies and installs the CUDA package matching this build.
pub async fn install_cuda(install_dir: &Path, data_dir: &Path) -> anyhow::Result<()> {
    let info = BuildInfo::current();
    let public_key = info
        .public_key
        .clone()
        .context("this build cannot download the CUDA package (no release signing key); install it manually")?;
    let client = crate::net::download_client()?;
    let release = release::release_for_build(&client, &info).await?;
    let name = release::cuda_asset_name(&info.label());
    let archive = data_dir.join("downloads").join(&name);
    release::download_signed(&client, &release, &name, &public_key, &archive).await?;

    let install_dir = install_dir.to_path_buf();
    let archive_clone = archive.clone();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let staging = install_dir.join(".cuda-staging");
        let _ = std::fs::remove_dir_all(&staging);
        let files = release::extract_zip(&archive_clone, &staging)?;
        for relative in files {
            let target = install_dir.join(&relative);
            crate::updater::replace_file(&staging.join(&relative), &target)?;
        }
        let _ = std::fs::remove_dir_all(&staging);
        std::fs::write(
            install_dir.join(CUDA_MARKER),
            format!("{}\n", BuildInfo::current().label()),
        )?;
        Ok(())
    })
    .await??;
    let _ = tokio::fs::remove_file(&archive).await;
    info!("CUDA package installed");
    Ok(())
}
