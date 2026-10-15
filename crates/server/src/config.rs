//! Server configuration (TOML).

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use verbarelay_protocol::models::{self, ModelSpec};

/// The complete server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub server: ServerSection,
    #[serde(default)]
    pub tls: Option<TlsSection>,
    pub enrollment: EnrollmentSection,
    #[serde(default)]
    pub models: ModelsSection,
    #[serde(default)]
    pub scheduling: SchedulingSection,
    #[serde(default)]
    pub splitting: SplittingSection,
    #[serde(default)]
    pub rate_limit: RateLimitSection,
    #[serde(default)]
    pub logging: LoggingSection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Address to listen on, e.g. "0.0.0.0:8443".
    pub bind: SocketAddr,
    /// Directory for the database (and logs unless configured otherwise).
    pub data_dir: PathBuf,
    /// Use the X-Forwarded-For header for client addresses (only behind a trusted proxy).
    #[serde(default)]
    pub trust_forwarded_for: bool,
    /// Largest accepted upload in MiB.
    #[serde(default = "default_max_upload_mb")]
    pub max_upload_mb: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    /// PEM certificate chain.
    pub cert: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentSection {
    /// Master password in plain text. Prefer `master_password_hash`.
    #[serde(default)]
    pub master_password: Option<String>,
    /// Argon2 hash of the master password (create one with `verbarelay-server hash-password`).
    #[serde(default)]
    pub master_password_hash: Option<String>,
    /// Peers that have not connected for this many days lose their token.
    #[serde(default = "default_ttl_days")]
    pub peer_token_ttl_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelsSection {
    /// The server dictates the model peers use.
    #[serde(default = "yes")]
    pub enforce: bool,
    /// The model peers use by default.
    #[serde(default = "default_model")]
    pub default: String,
    /// API requests may choose a different (allowed) model.
    #[serde(default = "yes")]
    pub allow_request_override: bool,
    /// Models requests may ask for.
    #[serde(default = "default_allowed")]
    pub allowed: Vec<String>,
    /// Models that are not in the built-in catalog.
    #[serde(default)]
    pub custom: Vec<ModelSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchedulingSection {
    /// How long a peer may take to accept a job offer.
    #[serde(default = "default_offer_timeout")]
    pub offer_timeout_secs: u64,
    /// How long a job may wait for a free peer.
    #[serde(default = "default_queue_timeout")]
    pub queue_timeout_secs: u64,
    /// How often a job is handed to another peer after failures.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// A job may take at most audio length times this factor (but at least two minutes).
    #[serde(default = "default_job_timeout_factor")]
    pub job_timeout_factor: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SplittingSection {
    /// Split long recordings at pauses and transcribe the parts on several peers.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Only recordings at least this long are split.
    #[serde(default = "default_min_duration")]
    pub min_duration_secs: u64,
    /// No part is shorter than this.
    #[serde(default = "default_min_part")]
    pub min_part_secs: u64,
    #[serde(default = "default_max_parts")]
    pub max_parts: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RateLimitSection {
    /// Sustained requests per minute and API token.
    #[serde(default = "default_rpm")]
    pub requests_per_minute: u32,
    /// Requests an API token may send in a burst.
    #[serde(default = "default_burst")]
    pub burst: u32,
    /// Jobs per API token that may be queued or running at once.
    #[serde(default = "default_active_jobs")]
    pub max_active_jobs_per_token: u32,
    /// Enrollment attempts per minute and client address.
    #[serde(default = "default_enroll_rpm")]
    pub enroll_attempts_per_minute: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingSection {
    /// trace, debug, info, warn or error (RUST_LOG overrides it).
    #[serde(default = "default_log_level")]
    pub level: String,
    /// "text" or "json".
    #[serde(default = "default_log_format")]
    pub format: String,
    /// Directory for daily rotated log files (default: <data_dir>/logs).
    #[serde(default)]
    pub dir: Option<PathBuf>,
}

fn yes() -> bool {
    true
}
fn default_max_upload_mb() -> u64 {
    512
}
fn default_ttl_days() -> u32 {
    14
}
fn default_model() -> String {
    models::DEFAULT_MODEL.to_string()
}
fn default_allowed() -> Vec<String> {
    ["large-v3-turbo", "large-v3", "medium", "small", "base", "tiny"]
        .map(String::from)
        .to_vec()
}
fn default_offer_timeout() -> u64 {
    5
}
fn default_queue_timeout() -> u64 {
    300
}
fn default_max_attempts() -> u32 {
    3
}
fn default_job_timeout_factor() -> f64 {
    4.0
}
fn default_min_duration() -> u64 {
    600
}
fn default_min_part() -> u64 {
    180
}
fn default_max_parts() -> usize {
    8
}
fn default_rpm() -> u32 {
    30
}
fn default_burst() -> u32 {
    10
}
fn default_active_jobs() -> u32 {
    4
}
fn default_enroll_rpm() -> u32 {
    5
}
fn default_log_level() -> String {
    "info".to_string()
}
fn default_log_format() -> String {
    "text".to_string()
}

impl Default for ModelsSection {
    fn default() -> Self {
        Self {
            enforce: true,
            default: default_model(),
            allow_request_override: true,
            allowed: default_allowed(),
            custom: Vec::new(),
        }
    }
}

impl Default for SchedulingSection {
    fn default() -> Self {
        Self {
            offer_timeout_secs: default_offer_timeout(),
            queue_timeout_secs: default_queue_timeout(),
            max_attempts: default_max_attempts(),
            job_timeout_factor: default_job_timeout_factor(),
        }
    }
}

impl Default for SplittingSection {
    fn default() -> Self {
        Self {
            enabled: true,
            min_duration_secs: default_min_duration(),
            min_part_secs: default_min_part(),
            max_parts: default_max_parts(),
        }
    }
}

impl Default for RateLimitSection {
    fn default() -> Self {
        Self {
            requests_per_minute: default_rpm(),
            burst: default_burst(),
            max_active_jobs_per_token: default_active_jobs(),
            enroll_attempts_per_minute: default_enroll_rpm(),
        }
    }
}

impl Default for LoggingSection {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            format: default_log_format(),
            dir: None,
        }
    }
}

impl ServerConfig {
    /// Reads and validates a configuration file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let config: ServerConfig = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Checks values that serde cannot check.
    pub fn validate(&self) -> anyhow::Result<()> {
        let e = &self.enrollment;
        match (&e.master_password, &e.master_password_hash) {
            (None, None) => bail!("enrollment: set master_password or master_password_hash"),
            (Some(_), Some(_)) => bail!("enrollment: set only one of master_password and master_password_hash"),
            (Some(p), None) if p.len() < 12 => {
                bail!("enrollment: the master password must be at least 12 characters long")
            }
            _ => {}
        }
        if e.peer_token_ttl_days == 0 {
            bail!("enrollment: peer_token_ttl_days must be at least 1");
        }
        if self.model(&self.models.default).is_none() {
            bail!("models: unknown default model '{}'", self.models.default);
        }
        for name in &self.models.allowed {
            if self.model(name).is_none() {
                bail!("models: unknown model '{name}' in allowed");
            }
        }
        for custom in &self.models.custom {
            if custom.sha256.len() != 64 || !custom.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
                bail!("models: custom model '{}' needs a 64 character hex sha256", custom.name);
            }
            if !custom.url.starts_with("https://") {
                bail!("models: custom model '{}' must be downloaded over https", custom.name);
            }
        }
        if self.splitting.max_parts == 0 || self.splitting.min_part_secs == 0 {
            bail!("splitting: max_parts and min_part_secs must be at least 1");
        }
        if self.rate_limit.requests_per_minute == 0 || self.rate_limit.burst == 0 {
            bail!("rate_limit: requests_per_minute and burst must be at least 1");
        }
        if !matches!(self.logging.format.as_str(), "text" | "json") {
            bail!("logging: format must be 'text' or 'json'");
        }
        Ok(())
    }

    /// Resolves a model name against the custom models and the built-in catalog.
    pub fn model(&self, name: &str) -> Option<ModelSpec> {
        self.models
            .custom
            .iter()
            .find(|m| m.name == name)
            .cloned()
            .or_else(|| models::find(name))
    }

    /// Models API requests may choose (the default model is always included).
    pub fn allowed_models(&self) -> Vec<ModelSpec> {
        let mut names: Vec<&String> = self.models.allowed.iter().collect();
        if !names.contains(&&self.models.default) {
            names.push(&self.models.default);
        }
        names.into_iter().filter_map(|n| self.model(n)).collect()
    }

    pub fn database_path(&self) -> PathBuf {
        self.server.data_dir.join("verbarelay.sqlite3")
    }

    pub fn log_dir(&self) -> PathBuf {
        self.logging
            .dir
            .clone()
            .unwrap_or_else(|| self.server.data_dir.join("logs"))
    }
}

/// A commented example configuration, written by `init-config`.
pub const EXAMPLE_CONFIG: &str = include_str!("../../../config/server.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_is_valid() {
        let config: ServerConfig = toml::from_str(EXAMPLE_CONFIG).unwrap();
        config.validate().unwrap();
        assert!(config.models.enforce);
        assert!(config.models.allow_request_override);
        assert_eq!(config.enrollment.peer_token_ttl_days, 14);
    }

    #[test]
    fn rejects_short_password_and_unknown_models() {
        let mut config: ServerConfig = toml::from_str(EXAMPLE_CONFIG).unwrap();
        config.enrollment.master_password = Some("short".into());
        assert!(config.validate().is_err());

        let mut config: ServerConfig = toml::from_str(EXAMPLE_CONFIG).unwrap();
        config.models.default = "nope".into();
        assert!(config.validate().is_err());
    }
}
