//! Local control channel of the running agent.
//!
//! The agent listens on a random localhost port and writes the port and a random
//! secret to `<data dir>/run/agent.json` (readable only by the user). A second
//! start of the client uses it to detect the running agent, to ask it to reload
//! the configuration after the settings were changed, and to show its status.
//! The tray icon also uses it to stop the agent and to check for updates.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use rand::TryRngCore;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::config::{Paths, write_private};

#[derive(Debug, Serialize, Deserialize)]
struct Endpoint {
    port: u16,
    secret: String,
    pid: u32,
}

/// What the agent reports about itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentStatus {
    pub version: String,
    pub server: String,
    pub connected: bool,
    pub backend: String,
    pub accelerator: String,
    pub model: Option<String>,
    pub score: Option<f64>,
    pub working: bool,
    pub jobs_done: u64,
    pub last_error: Option<String>,
    pub pending_update: Option<String>,
}

pub type SharedStatus = Arc<Mutex<AgentStatus>>;

/// Commands the agent accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Reload,
}

/// A request to check for updates now, answered with a sentence for the user.
pub type UpdateCheck = oneshot::Sender<String>;

/// Where the control channel forwards requests.
#[derive(Clone)]
pub struct Controls {
    pub commands: mpsc::Sender<Command>,
    pub update_checks: mpsc::Sender<UpdateCheck>,
    /// Cancelled when the client is asked to stop.
    pub quit: CancellationToken,
}

fn endpoint_file(paths: &Paths) -> PathBuf {
    paths.runtime_dir.join("agent.json")
}

/// Starts the control listener. Returns a guard that removes the endpoint file.
pub async fn serve(paths: &Paths, status: SharedStatus, controls: Controls) -> anyhow::Result<EndpointGuard> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let mut secret_bytes = [0u8; 24];
    rand::rngs::OsRng.try_fill_bytes(&mut secret_bytes)?;
    let secret: String = secret_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let file = endpoint_file(paths);
    let endpoint = Endpoint {
        port,
        secret: secret.clone(),
        pid: std::process::id(),
    };
    write_private(&file, serde_json::to_string(&endpoint)?.as_bytes())?;

    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let secret = secret.clone();
            let status = status.clone();
            let controls = controls.clone();
            tokio::spawn(async move {
                if let Err(e) = handle(stream, &secret, &status, &controls).await {
                    debug!("control connection: {e:#}");
                }
            });
        }
    });
    Ok(EndpointGuard { file })
}

async fn handle(stream: TcpStream, secret: &str, status: &SharedStatus, controls: &Controls) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    tokio::time::timeout(Duration::from_secs(5), BufReader::new(read).read_line(&mut line)).await??;
    let mut parts = line.split_whitespace();
    if parts.next() != Some(secret) {
        warn!("control connection with a wrong secret");
        return Ok(());
    }
    let reply = match parts.next() {
        Some("ping") => "pong".to_string(),
        Some("status") => serde_json::to_string(&*status.lock().unwrap())?,
        Some("reload") => {
            controls.commands.send(Command::Reload).await?;
            "ok".to_string()
        }
        Some("check-updates") => {
            let (tx, rx) = oneshot::channel();
            let answer = match controls.update_checks.send(tx).await {
                Ok(()) => rx.await.ok(),
                Err(_) => None,
            };
            answer
                .unwrap_or_else(|| "The update check is not available right now.".into())
                .replace('\n', " ")
        }
        Some("quit") => {
            write.write_all(b"ok\n").await?;
            controls.quit.cancel();
            return Ok(());
        }
        _ => "unknown command".to_string(),
    };
    write.write_all(format!("{reply}\n").as_bytes()).await?;
    Ok(())
}

/// Removes the endpoint file when the agent stops.
pub struct EndpointGuard {
    file: PathBuf,
}

impl Drop for EndpointGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.file);
    }
}

/// Sends a command to the running agent and returns its reply.
pub async fn send(paths: &Paths, command: &str) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(endpoint_file(paths)).context("the client is not running")?;
    let endpoint: Endpoint = serde_json::from_str(&text)?;
    let mut stream = tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(("127.0.0.1", endpoint.port)))
        .await
        .context("the client is not responding")??;
    stream
        .write_all(format!("{} {command}\n", endpoint.secret).as_bytes())
        .await?;
    // Checking for updates waits for GitHub.
    let timeout = Duration::from_secs(if command == "check-updates" { 90 } else { 5 });
    let mut reply = String::new();
    tokio::time::timeout(timeout, BufReader::new(stream).read_line(&mut reply)).await??;
    if reply.trim() == "unknown command" {
        bail!("the running client does not understand '{command}'");
    }
    Ok(reply.trim().to_string())
}

/// Blocking helper for code outside a runtime.
pub fn send_blocking(paths: &Paths, command: &str) -> anyhow::Result<String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(send(paths, command))
}

/// Whether an agent is running for this user.
pub fn agent_running(paths: &Paths) -> bool {
    send_blocking(paths, "ping").is_ok_and(|r| r == "pong")
}

/// Reads the status of the running agent.
pub fn agent_status(paths: &Paths) -> anyhow::Result<AgentStatus> {
    Ok(serde_json::from_str(&send_blocking(paths, "status")?)?)
}
