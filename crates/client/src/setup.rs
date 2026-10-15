//! First-run setup shared by the setup window and the terminal setup:
//! hardware detection with a short speed probe, enrollment with the master
//! password and the optional CUDA download.

use std::time::Duration;

use anyhow::{Context, bail};
use verbarelay_protocol::messages::{Backend, EnrollRequest, EnrollResponse};
use verbarelay_protocol::models;

use crate::config::{ClientConfig, Credentials, Paths, normalize_server_url};
use crate::detect::{self, BackendOption};
use crate::models::ModelStore;

/// Result of the hardware detection.
#[derive(Debug, Clone)]
pub struct Detection {
    pub options: Vec<BackendOption>,
    pub note: Option<String>,
}

/// Detects devices and, if `probe` is set, measures every backend that works
/// without a download using the tiny model (about 75 MB, downloaded once).
pub async fn detect(paths: &Paths, probe: bool, progress: &(dyn Fn(String) + Sync)) -> Detection {
    progress("Looking for graphics cards...".into());
    let (devices, mut note) = match crate::worker::detect_devices().await {
        Ok(d) => (d, None),
        Err(e) => (
            Vec::new(),
            Some(format!("Device detection failed ({e:#}); only the CPU is offered.")),
        ),
    };
    let cuda_installed = crate::packages::cuda_installed(&paths.install_dir);
    let mut measured = Vec::new();
    if probe {
        let candidates: Vec<Backend> = detect::backend_options(&devices, cuda_installed, &[])
            .into_iter()
            .filter(|o| !o.needs_download)
            .map(|o| o.backend)
            .collect();
        if candidates.len() > 1 {
            match probe_speeds(paths, &candidates, progress).await {
                Ok(m) => measured = m,
                Err(e) => note = Some(format!("The speed test was skipped: {e:#}")),
            }
        }
    }
    let options = detect::backend_options(&devices, cuda_installed, &measured);
    progress("Detection finished.".into());
    Detection { options, note }
}

async fn probe_speeds(
    paths: &Paths,
    backends: &[Backend],
    progress: &(dyn Fn(String) + Sync),
) -> anyhow::Result<Vec<(Backend, f64)>> {
    let spec = models::find(models::PROBE_MODEL).context("probe model missing from catalog")?;
    let store = ModelStore::new(paths.models_dir.clone());
    if !store.has(&spec) {
        progress("Downloading a small test model (75 MB) to compare speeds...".into());
    }
    let client = crate::net::download_client()?;
    let path = store.ensure(&client, &spec).await?;
    let mut results = Vec::new();
    for &backend in backends {
        progress(format!("Measuring the speed of {}...", backend.display_name()));
        match tokio::time::timeout(
            Duration::from_secs(180),
            crate::benchmark::run(&path, &spec.name, backend, 0),
        )
        .await
        {
            Ok(Ok(result)) => results.push((backend, result.speed())),
            Ok(Err(e)) => tracing::warn!(%backend, "probe failed: {e:#}"),
            Err(_) => tracing::warn!(%backend, "probe timed out"),
        }
    }
    Ok(results)
}

/// Exchanges the master password for a token.
pub async fn enroll(config: &ClientConfig, master_password: &str, public_key: &str) -> anyhow::Result<EnrollResponse> {
    let client = crate::net::server_client(config)?;
    let url = format!("{}{}", config.base_url()?, verbarelay_protocol::PEER_ENROLL_PATH);
    let request = EnrollRequest {
        master_password: master_password.to_string(),
        name: config.device_name.clone(),
        public_key: public_key.to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let response = client.post(&url).json(&request).send().await.map_err(|e| {
        if e.is_connect() || e.is_timeout() {
            anyhow::anyhow!(
                "the server could not be reached at {}",
                config.base_url().unwrap_or_default()
            )
        } else {
            anyhow::anyhow!("the connection to the server failed: {e}")
        }
    })?;
    match response.status().as_u16() {
        200 => Ok(response.json().await?),
        401 => bail!("the master password is wrong"),
        429 => bail!("too many attempts; wait a minute and try again"),
        400 => bail!(
            "the server rejected the request: {}",
            response.text().await.unwrap_or_default()
        ),
        code => bail!("the server answered with status {code}"),
    }
}

/// Everything the user entered.
#[derive(Debug, Clone)]
pub struct SetupInput {
    pub server_url: String,
    pub master_password: String,
    pub device_name: String,
    pub backend: Backend,
    pub allow_insecure: bool,
}

/// Enrolls, installs the CUDA package if needed and saves the configuration.
pub async fn complete(
    paths: &Paths,
    input: SetupInput,
    progress: &(dyn Fn(String) + Sync),
) -> anyhow::Result<ClientConfig> {
    let url = normalize_server_url(&input.server_url, input.allow_insecure)?;
    let name = input.device_name.trim();
    if name.is_empty() {
        bail!("enter a device name");
    }
    // Keep update settings and tuning of an earlier configuration.
    let mut config = ClientConfig::load_optional(&paths.config_file)
        .ok()
        .flatten()
        .unwrap_or_else(|| ClientConfig::new(url.clone(), name.to_string(), input.backend));
    config.server_url = url;
    config.device_name = name.to_string();
    config.backend = input.backend;
    config.allow_insecure = input.allow_insecure;
    config.credentials = None;

    progress("Connecting to the server...".into());
    let keys = paths.regenerate_keypair()?;
    let public_key = verbarelay_protocol::crypto::encode_key(&keys.public_bytes());
    let response = enroll(&config, &input.master_password, &public_key).await?;
    config.credentials = Some(Credentials {
        peer_id: response.peer_id,
        token: response.token,
    });

    if input.backend == Backend::Cuda && !crate::packages::cuda_installed(&paths.install_dir) {
        progress("Downloading the CUDA runtime. This can take a few minutes...".into());
        crate::packages::install_cuda(&paths.install_dir, &paths.data_dir)
            .await
            .context("installing the CUDA package failed")?;
    }
    config.save(&paths.config_file)?;
    progress("Setup complete.".into());
    Ok(config)
}

/// Terminal setup for machines without a desktop.
pub fn run_terminal(paths: &Paths) -> anyhow::Result<Option<ClientConfig>> {
    use std::io::{BufRead, Write};
    let runtime = tokio::runtime::Runtime::new()?;
    let stdin = std::io::stdin();
    let ask = |prompt: &str, default: &str| -> anyhow::Result<String> {
        if default.is_empty() {
            print!("{prompt}: ")
        } else {
            print!("{prompt} [{default}]: ")
        }
        std::io::stdout().flush()?;
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        let line = line.trim();
        Ok(if line.is_empty() {
            default.to_string()
        } else {
            line.to_string()
        })
    };

    println!("VerbaRelay client setup");
    let server_url = ask("Server address (for example https://transcribe.example.com:8443)", "")?;
    let (hostname, _, _, _) = detect::host_info();
    let device_name = ask("Device name", &hostname)?;
    let master_password = rpassword::prompt_password("Master password: ")?;

    println!("Detecting hardware and measuring speed. This takes a minute...");
    let detection = runtime.block_on(detect(paths, true, &|msg| println!("  {msg}")));
    if let Some(note) = &detection.note {
        println!("{note}");
    }
    println!("Available compute backends:");
    for (i, option) in detection.options.iter().enumerate() {
        println!("  {}. {}", i + 1, option.label());
    }
    let default_index = detection.options.iter().position(|o| o.recommended).unwrap_or(0) + 1;
    let choice = ask("Choose a number", &default_index.to_string())?;
    let index: usize = choice.parse().context("enter one of the numbers")?;
    let option = detection
        .options
        .get(index.wrapping_sub(1))
        .context("enter one of the numbers")?;

    let input = SetupInput {
        allow_insecure: server_url.trim().starts_with("http://"),
        server_url,
        master_password,
        device_name,
        backend: option.backend,
    };
    let config = runtime.block_on(complete(paths, input, &|msg| println!("{msg}")))?;
    println!("Configuration saved to {}", paths.config_file.display());
    Ok(Some(config))
}
