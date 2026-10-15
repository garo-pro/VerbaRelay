//! TLS and HTTP setup shared by enrollment, downloads, the updater and the peer connection.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use rustls::ClientConfig as TlsConfig;

use crate::config::ClientConfig;

/// User agent of all requests.
pub fn user_agent() -> String {
    format!("verbarelay-client/{}", env!("CARGO_PKG_VERSION"))
}

/// Builds the TLS configuration: public web roots plus an optional private CA.
pub fn tls_config(extra_ca: Option<&std::path::Path>) -> anyhow::Result<Arc<TlsConfig>> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = extra_ca {
        let pem = std::fs::read(path).with_context(|| format!("reading CA certificate {}", path.display()))?;
        for cert in rustls_pemfile::certs(&mut pem.as_slice()) {
            roots
                .add(cert.context("parsing CA certificate")?)
                .context("adding CA certificate")?;
        }
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = TlsConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring TLS")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// An HTTP client for talking to the VerbaRelay server.
pub fn server_client(config: &ClientConfig) -> anyhow::Result<reqwest::Client> {
    http_client(Some(config), Duration::from_secs(30))
}

/// An HTTP client for public downloads (models, updates). No overall timeout.
pub fn download_client() -> anyhow::Result<reqwest::Client> {
    http_client(None, Duration::from_secs(0))
}

fn http_client(config: Option<&ClientConfig>, timeout: Duration) -> anyhow::Result<reqwest::Client> {
    let tls = tls_config(config.and_then(|c| c.ca_certificate.as_deref()))?;
    let mut builder = reqwest::Client::builder()
        .user_agent(user_agent())
        .use_preconfigured_tls((*tls).clone())
        .connect_timeout(Duration::from_secs(20))
        .https_only(!config.is_some_and(|c| c.allow_insecure));
    if !timeout.is_zero() {
        builder = builder.timeout(timeout);
    }
    Ok(builder.build()?)
}
