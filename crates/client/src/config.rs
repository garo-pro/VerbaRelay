//! Client configuration and file locations.
//!
//! * Configuration: `<config dir>/client.toml` (server, token, backend, update settings)
//! * Private key:   `<config dir>/peer.key` (never leaves the device)
//! * Models, logs:  `<data dir>/models`, `<data dir>/logs`
//! * Program files: the directory of the executable (native libraries live next to it)

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use verbarelay_protocol::crypto::PeerKeypair;
use verbarelay_protocol::messages::Backend;
use verbarelay_protocol::models;
use zeroize::Zeroizing;

/// The client configuration file.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// Base URL of the server, e.g. "https://transcribe.example.com:8443".
    pub server_url: String,
    /// Name shown on the server.
    pub device_name: String,
    /// Compute backend: "cuda", "vulkan" or "cpu".
    pub backend: Backend,
    /// Model used when the server does not dictate one.
    #[serde(default = "default_model")]
    pub model: String,
    /// CPU threads for transcription (0 = automatic).
    #[serde(default)]
    pub threads: u32,
    /// Additional CA certificate (PEM) to trust, for servers with a private CA.
    #[serde(default)]
    pub ca_certificate: Option<PathBuf>,
    /// Allow plain http:// servers. Only for testing; audio stays encrypted per job.
    #[serde(default)]
    pub allow_insecure: bool,
    /// Set by enrollment.
    #[serde(default)]
    pub credentials: Option<Credentials>,
    #[serde(default)]
    pub updates: UpdateSettings,
}

/// What the server handed out during enrollment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub peer_id: Uuid,
    pub token: String,
}

/// The only user-facing settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettings {
    /// Hours between update checks.
    #[serde(default = "default_interval")]
    pub check_interval_hours: u32,
    /// Also install development builds.
    #[serde(default)]
    pub include_dev_builds: bool,
}

fn default_model() -> String {
    models::DEFAULT_MODEL.to_string()
}

fn default_interval() -> u32 {
    6
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self {
            check_interval_hours: default_interval(),
            include_dev_builds: false,
        }
    }
}

impl ClientConfig {
    pub fn new(server_url: String, device_name: String, backend: Backend) -> Self {
        Self {
            server_url,
            device_name,
            backend,
            model: default_model(),
            threads: 0,
            ca_certificate: None,
            allow_insecure: false,
            credentials: None,
            updates: UpdateSettings::default(),
        }
    }

    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Loads the configuration if it exists.
    pub fn load_optional(path: &Path) -> anyhow::Result<Option<Self>> {
        if path.exists() {
            Self::load(path).map(Some)
        } else {
            Ok(None)
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        normalize_server_url(&self.server_url, self.allow_insecure)?;
        if self.updates.check_interval_hours == 0 || self.updates.check_interval_hours > 24 * 30 {
            bail!("updates.check_interval_hours must be between 1 and 720");
        }
        Ok(())
    }

    /// Writes the configuration atomically with owner-only permissions.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let text = format!(
            "# VerbaRelay client configuration. Written by the setup; editing by hand is fine.\n{}",
            toml::to_string_pretty(self)?
        );
        write_private(path, text.as_bytes())
    }

    /// The server base URL without a trailing slash.
    pub fn base_url(&self) -> anyhow::Result<String> {
        normalize_server_url(&self.server_url, self.allow_insecure)
    }

    /// The WebSocket URL of the peer endpoint.
    pub fn connect_url(&self) -> anyhow::Result<String> {
        let base = self.base_url()?;
        let ws = if let Some(rest) = base.strip_prefix("https://") {
            format!("wss://{rest}")
        } else if let Some(rest) = base.strip_prefix("http://") {
            format!("ws://{rest}")
        } else {
            bail!("unsupported server URL");
        };
        Ok(format!("{ws}{}", verbarelay_protocol::PEER_CONNECT_PATH))
    }
}

/// Checks and normalizes a server URL ("host:port" becomes "https://host:port").
pub fn normalize_server_url(url: &str, allow_insecure: bool) -> anyhow::Result<String> {
    let url = url.trim().trim_end_matches('/');
    if url.is_empty() {
        bail!("the server address is empty");
    }
    let url = if url.contains("://") {
        url.to_string()
    } else {
        format!("https://{url}")
    };
    if url.starts_with("http://") && !allow_insecure {
        bail!("the server address must use https:// (set allow_insecure = true only for testing)");
    }
    if !url.starts_with("https://") && !url.starts_with("http://") {
        bail!("the server address must start with https://");
    }
    let host = url.split("://").nth(1).unwrap_or_default();
    if host.is_empty() || host.contains(char::is_whitespace) {
        bail!("the server address is not valid");
    }
    Ok(url)
}

/// Writes a file atomically; on Unix only the owner may read it.
pub fn write_private(path: &Path, contents: &[u8]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(contents)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

/// File locations of this installation.
#[derive(Debug, Clone)]
pub struct Paths {
    pub config_file: PathBuf,
    pub key_file: PathBuf,
    pub data_dir: PathBuf,
    pub models_dir: PathBuf,
    pub logs_dir: PathBuf,
    pub runtime_dir: PathBuf,
    /// Directory of the executable and the native libraries.
    pub install_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> anyhow::Result<Self> {
        let dirs = directories::ProjectDirs::from("", "", "verbarelay").context("no home directory found")?;
        let exe = std::env::current_exe().context("locating the executable")?;
        let install_dir = exe.parent().context("executable has no directory")?.to_path_buf();
        let config_dir =
            std::env::var_os("VERBARELAY_CONFIG_DIR").map_or_else(|| dirs.config_dir().to_path_buf(), PathBuf::from);
        let data_dir =
            std::env::var_os("VERBARELAY_DATA_DIR").map_or_else(|| dirs.data_local_dir().to_path_buf(), PathBuf::from);
        Ok(Self {
            config_file: config_dir.join("client.toml"),
            key_file: config_dir.join("peer.key"),
            models_dir: data_dir.join("models"),
            logs_dir: data_dir.join("logs"),
            runtime_dir: data_dir.join("run"),
            data_dir,
            install_dir,
        })
    }

    /// Loads the peer key pair, creating it on first use.
    pub fn load_or_create_keypair(&self) -> anyhow::Result<PeerKeypair> {
        if self.key_file.exists() {
            let text = Zeroizing::new(std::fs::read_to_string(&self.key_file)?);
            let bytes = verbarelay_protocol::crypto::decode_key(&text).context("the peer key file is damaged")?;
            return Ok(PeerKeypair::from_secret_bytes(bytes));
        }
        let pair = PeerKeypair::generate();
        let encoded = Zeroizing::new(verbarelay_protocol::crypto::encode_key(&pair.secret_bytes()));
        write_private(&self.key_file, encoded.as_bytes())?;
        Ok(pair)
    }

    /// Replaces the key pair (used when enrolling again).
    pub fn regenerate_keypair(&self) -> anyhow::Result<PeerKeypair> {
        let _ = std::fs::remove_file(&self.key_file);
        self.load_or_create_keypair()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization() {
        assert_eq!(
            normalize_server_url("example.com:8443/", false).unwrap(),
            "https://example.com:8443"
        );
        assert!(normalize_server_url("http://example.com", false).is_err());
        assert_eq!(
            normalize_server_url("http://localhost:8080", true).unwrap(),
            "http://localhost:8080"
        );
        assert!(normalize_server_url("", false).is_err());
        assert!(normalize_server_url("ftp://x", false).is_err());
    }

    #[test]
    fn connect_url() {
        let config = ClientConfig::new("https://example.com:8443".into(), "pc".into(), Backend::Cpu);
        assert_eq!(
            config.connect_url().unwrap(),
            "wss://example.com:8443/api/v1/peers/connect"
        );
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client.toml");
        let mut config = ClientConfig::new("example.com".into(), "pc".into(), Backend::Cuda);
        config.credentials = Some(Credentials {
            peer_id: Uuid::new_v4(),
            token: "vrp_x".into(),
        });
        config.save(&path).unwrap();
        let loaded = ClientConfig::load(&path).unwrap();
        assert_eq!(loaded.backend, Backend::Cuda);
        assert_eq!(loaded.credentials.unwrap().token, "vrp_x");
        assert_eq!(loaded.updates, UpdateSettings::default());
    }
}
