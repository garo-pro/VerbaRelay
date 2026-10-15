//! VerbaRelay server: distributes transcription jobs to peer devices.

mod api;
mod app;
mod audio;
mod auth;
mod config;
mod db;
mod formats;
mod logging;
mod peers;
mod rate_limit;
mod scheduler;
mod stats;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::ServerConfig;
use crate::db::Db;

#[derive(Parser)]
#[command(
    name = "verbarelay-server",
    version,
    about = "Distributes audio transcription jobs to VerbaRelay peers"
)]
struct Cli {
    /// Path of the configuration file.
    #[arg(
        short,
        long,
        env = "VERBARELAY_CONFIG",
        default_value = "/etc/verbarelay/server.toml",
        global = true
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server.
    Serve,
    /// Write a commented example configuration file.
    InitConfig {
        /// Target file (defaults to --config).
        path: Option<PathBuf>,
    },
    /// Check the configuration file and exit.
    CheckConfig,
    /// Read a password from the terminal and print its Argon2 hash for `master_password_hash`.
    HashPassword,
    /// Manage enrolled peers.
    #[command(subcommand)]
    Peers(PeersCommand),
    /// Manage API tokens for callers that submit audio.
    #[command(subcommand)]
    Tokens(TokensCommand),
    /// Show statistics.
    Stats {
        /// Period in days.
        #[arg(long, default_value_t = 7)]
        days: u32,
        /// Print JSON instead of text.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum PeersCommand {
    /// List enrolled peers.
    List,
    /// Remove a peer; it has to be set up again with the master password.
    Revoke { id: Uuid },
}

#[derive(Subcommand)]
enum TokensCommand {
    /// Create a token. It is shown only once.
    Create {
        /// A name that tells you who uses the token.
        name: String,
        /// Let the token expire after this many days.
        #[arg(long)]
        expires_days: Option<u32>,
    },
    /// List tokens.
    List,
    /// Revoke a token.
    Revoke { id: Uuid },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let config = ServerConfig::load(&cli.config)?;
            let _guard = logging::init(&config)?;
            tokio::runtime::Runtime::new()?.block_on(serve(config))
        }
        Command::InitConfig { path } => {
            let path = path.unwrap_or(cli.config);
            if path.exists() {
                bail!("{} already exists", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, config::EXAMPLE_CONFIG)?;
            restrict_permissions(&path);
            println!(
                "Wrote {}. Change the master password before starting the server.",
                path.display()
            );
            Ok(())
        }
        Command::CheckConfig => {
            let config = ServerConfig::load(&cli.config)?;
            println!("Configuration is valid.");
            println!("Listening on: {}", config.server.bind);
            println!(
                "TLS: {}",
                if config.tls.is_some() {
                    "enabled"
                } else {
                    "disabled (use a reverse proxy)"
                }
            );
            println!(
                "Model: {} ({})",
                config.models.default,
                if config.models.enforce {
                    "enforced"
                } else {
                    "peers choose"
                }
            );
            println!(
                "Requests may override the model: {}",
                if config.models.allow_request_override {
                    "yes"
                } else {
                    "no"
                }
            );
            Ok(())
        }
        Command::HashPassword => {
            let password = rpassword::prompt_password("Master password: ")?;
            if password.len() < 12 {
                bail!("use at least 12 characters");
            }
            let again = rpassword::prompt_password("Repeat: ")?;
            if password != again {
                bail!("the passwords do not match");
            }
            println!("{}", auth::hash_password(&password)?);
            Ok(())
        }
        Command::Peers(command) => {
            logging::init_cli();
            let db = open_db(&cli.config)?;
            match command {
                PeersCommand::List => list_peers(&db),
                PeersCommand::Revoke { id } => {
                    if db.delete_peer(id)? {
                        println!("Peer {id} revoked. A connected peer is disconnected within five minutes.");
                    } else {
                        println!("No peer with id {id}.");
                    }
                    Ok(())
                }
            }
        }
        Command::Tokens(command) => {
            logging::init_cli();
            let db = open_db(&cli.config)?;
            match command {
                TokensCommand::Create { name, expires_days } => {
                    let id = Uuid::new_v4();
                    let token = auth::generate_token(verbarelay_protocol::API_TOKEN_PREFIX);
                    let expires = expires_days.map(|d| db::now() + i64::from(d) * 86_400);
                    db.insert_api_token(id, &name, &auth::hash_token(&token), expires)?;
                    println!("Token for {name} (id {id}):");
                    println!("{token}");
                    println!("Store it now; it cannot be shown again.");
                    Ok(())
                }
                TokensCommand::List => {
                    let tokens = db.list_api_tokens()?;
                    if tokens.is_empty() {
                        println!("No API tokens. Create one with: verbarelay-server tokens create <name>");
                    }
                    for t in tokens {
                        let state = if t.revoked {
                            "revoked".to_string()
                        } else if t.expires_at.is_some_and(|e| e <= db::now()) {
                            "expired".to_string()
                        } else {
                            "active".to_string()
                        };
                        println!("{} ({state})", t.name);
                        println!("  Id: {}", t.id);
                        println!("  Created: {}", stats::format_time(t.created_at));
                        println!(
                            "  Last used: {}",
                            t.last_used.map_or_else(|| "never".into(), stats::format_time)
                        );
                        if let Some(e) = t.expires_at {
                            println!("  Expires: {}", stats::format_time(e));
                        }
                    }
                    Ok(())
                }
                TokensCommand::Revoke { id } => {
                    if db.revoke_api_token(id)? {
                        println!("Token {id} revoked.");
                    } else {
                        println!("No token with id {id}.");
                    }
                    Ok(())
                }
            }
        }
        Command::Stats { days, json } => {
            logging::init_cli();
            let db = open_db(&cli.config)?;
            let report = stats::collect(&db, days)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", stats::render(&report));
            }
            Ok(())
        }
    }
}

fn open_db(config_path: &std::path::Path) -> anyhow::Result<Db> {
    let config = ServerConfig::load(config_path)?;
    Db::open(&config.database_path())
}

fn list_peers(db: &Db) -> anyhow::Result<()> {
    let report = stats::collect(db, 7)?;
    if report.peers.is_empty() {
        println!("No peers enrolled.");
    }
    for p in report.peers {
        println!("{} ({})", p.name, if p.online { "online" } else { "offline" });
        println!("  Id: {}", p.id);
        println!("  Device: {} via {}", p.accelerator, p.backend);
        println!(
            "  Transcription score: {}",
            p.score.map_or_else(|| "not measured yet".into(), |s| format!("{s:.1}"))
        );
        println!("  Last seen: {}", stats::format_time(p.last_seen));
        println!("  Client version: {}", p.client_version);
    }
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &std::path::Path) {}

async fn serve(config: ServerConfig) -> anyhow::Result<()> {
    let config = Arc::new(config);
    let db = Arc::new(Db::open(&config.database_path())?);
    db.close_dangling_sessions()?;
    let scheduler = scheduler::Scheduler::new(config.clone(), db.clone());
    tokio::spawn(scheduler.clone().run());
    let state = app::AppState::new(config.clone(), db.clone(), scheduler);

    // Housekeeping: expire peer tokens and trim rate limiter memory.
    let housekeeping = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tick.tick().await;
            let ttl = i64::from(housekeeping.config.enrollment.peer_token_ttl_days) * 86_400;
            match housekeeping.db.expire_peers(db::now() - ttl) {
                Ok(ids) if !ids.is_empty() => {
                    info!(count = ids.len(), ?ids, "removed peers without connection for too long")
                }
                Ok(_) => {}
                Err(e) => warn!("expiring peers failed: {e:#}"),
            }
            housekeeping.api_limiter.cleanup();
            housekeeping.enroll_limiter.cleanup();
        }
    });

    let router = app::router(state).into_make_service_with_connect_info::<SocketAddr>();
    let addr = config.server.bind;
    let policy = if config.models.enforce {
        "enforced"
    } else {
        "chosen by peers"
    };
    info!(%addr, model = %config.models.default, policy, version = env!("CARGO_PKG_VERSION"), "starting VerbaRelay server");

    let handle = axum_server::Handle::new();
    let shutdown = handle.clone();
    tokio::spawn(async move {
        shutdown_signal().await;
        info!("shutting down");
        shutdown.graceful_shutdown(Some(Duration::from_secs(10)));
    });

    match &config.tls {
        Some(tls) => {
            let rustls = axum_server::tls_rustls::RustlsConfig::from_pem_file(&tls.cert, &tls.key)
                .await
                .with_context(|| format!("loading TLS certificate {}", tls.cert.display()))?;
            axum_server::bind_rustls(addr, rustls)
                .handle(handle)
                .serve(router)
                .await?;
        }
        None => {
            if !addr.ip().is_loopback() {
                warn!("TLS is disabled; only run like this behind a reverse proxy that terminates TLS");
            }
            axum_server::bind(addr).handle(handle).serve(router).await?;
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
