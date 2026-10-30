//! VerbaRelay client: a background peer that transcribes audio for a server.
//!
//! Started without arguments it runs in the background (setting itself up on the
//! first start). Starting it again while it runs opens the settings window.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod agent;
mod autostart;
mod benchmark;
mod config;
mod detect;
mod engine;
#[cfg(feature = "gui")]
mod gui;
mod ipc;
mod logging;
mod models;
mod net;
mod packages;
mod release;
mod setup;
mod tones;
#[cfg(feature = "gui")]
mod tray;
mod updater;
mod worker;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use verbarelay_protocol::messages::Backend;

use crate::agent::{Agent, AgentExit};
use crate::config::{ClientConfig, Paths};
use crate::release::BuildInfo;
use crate::tones::Tone;

#[derive(Parser)]
#[command(
    name = "verbarelay-client",
    version,
    about = "Transcribes audio in the background for a VerbaRelay server"
)]
struct Cli {
    /// Never open windows; use the terminal instead.
    #[arg(long, global = true)]
    headless: bool,
    /// Set after an automatic update (internal).
    #[arg(long, hide = true)]
    after_update: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the background client (the default).
    Run,
    /// Set up this device (again). Without options it asks interactively.
    Setup {
        /// Server address, for unattended setup.
        #[arg(long, requires = "backend")]
        server: Option<String>,
        /// Device name (defaults to the host name).
        #[arg(long)]
        name: Option<String>,
        /// Compute backend for unattended setup: cuda, vulkan, metal or cpu.
        #[arg(long, requires = "server")]
        backend: Option<Backend>,
        /// Master password for unattended setup (better: VERBARELAY_MASTER_PASSWORD).
        #[arg(long, env = "VERBARELAY_MASTER_PASSWORD", hide_env_values = true)]
        master_password: Option<String>,
    },
    /// Show the settings (updates and tones).
    Settings,
    /// Show the status of the running client.
    Status,
    /// Ask the running client to reload its configuration file.
    Reload,
    /// Measure the transcription speed with the configured model.
    Benchmark,
    /// Turn starting with the user session on or off.
    Autostart {
        #[arg(value_parser = ["enable", "disable"])]
        state: String,
    },
    /// Transcription worker process (internal).
    #[command(hide = true)]
    Worker {
        #[arg(long)]
        model: PathBuf,
        #[arg(long)]
        backend: Backend,
        #[arg(long, default_value_t = 0)]
        threads: u32,
    },
    /// Prints the compute devices as JSON (internal).
    #[command(hide = true)]
    DetectDevices,
    /// Tray icon of the running client (internal).
    #[command(hide = true)]
    Tray {
        /// Exit when stdin closes, i.e. when the agent that started the tray stops.
        #[arg(long)]
        attached: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = real_main(cli) {
        error!("{e:#}");
        eprintln!("Error: {e:#}");
        std::process::exit(1);
    }
}

fn gui_available(cli: &Cli) -> bool {
    #[cfg(feature = "gui")]
    {
        !cli.headless && gui::available()
    }
    #[cfg(not(feature = "gui"))]
    {
        let _ = cli;
        false
    }
}

fn real_main(mut cli: Cli) -> anyhow::Result<()> {
    let paths = Paths::detect()?;

    // Internal subcommands first: they must not touch logs or configuration.
    match &cli.command {
        Some(Command::Worker {
            model,
            backend,
            threads,
        }) => {
            return worker::run_worker(&paths.install_dir, model, *backend, *threads);
        }
        Some(Command::DetectDevices) => return worker::run_detect(&paths.install_dir),
        _ => {}
    }

    #[cfg(feature = "gui")]
    if let Some(Command::Tray { attached }) = cli.command {
        let _log_guard = logging::init(&paths.logs_dir, "tray");
        tray::run(paths, attached);
        return Ok(());
    }

    let _log_guard = logging::init(&paths.logs_dir, "client");
    match cli.command.take() {
        None | Some(Command::Run) => run_background(&cli, &paths),
        Some(Command::Setup {
            server,
            name,
            backend,
            master_password,
        }) => {
            if ipc::agent_running(&paths) {
                bail!("the client is running; quit it before setting it up again");
            }
            match (server, backend) {
                (Some(server_url), Some(backend)) => {
                    let master_password =
                        master_password.context("set VERBARELAY_MASTER_PASSWORD for unattended setup")?;
                    let device_name = name.unwrap_or_else(|| detect::host_info().0);
                    let input = setup::SetupInput {
                        allow_insecure: server_url.trim().starts_with("http://"),
                        server_url,
                        master_password,
                        device_name,
                        backend,
                    };
                    let config = tokio::runtime::Runtime::new()?
                        .block_on(setup::complete(&paths, input, &|m| println!("{m}")))?;
                    println!("Set up as '{}' on {}.", config.device_name, config.server_url);
                }
                _ => {
                    interactive_setup(&cli, &paths)?;
                }
            }
            Ok(())
        }
        Some(Command::Settings) => open_settings(&cli, &paths),
        Some(Command::Status) => {
            let status = ipc::agent_status(&paths)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
            Ok(())
        }
        Some(Command::Reload) => {
            ipc::send_blocking(&paths, "reload")?;
            println!("The running client reloaded its configuration.");
            Ok(())
        }
        Some(Command::Benchmark) => {
            let config = ClientConfig::load(&paths.config_file).context("the client is not set up yet")?;
            let store = models::ModelStore::new(paths.models_dir.clone());
            let model = store
                .path_of_name(&config.model)
                .with_context(|| format!("model {} is not downloaded yet", config.model))?;
            let result = tokio::runtime::Runtime::new()?.block_on(benchmark::run(
                &model,
                &config.model,
                config.backend,
                config.threads,
            ))?;
            println!("Model: {}", result.model);
            println!("Backend: {}", result.backend.display_name());
            println!("Speed: {:.1} times real time (transcription score)", result.speed());
            Ok(())
        }
        Some(Command::Autostart { state }) => {
            if state == "enable" {
                autostart::enable()?
            } else {
                autostart::disable()?
            }
            println!("Autostart {state}d.");
            Ok(())
        }
        Some(Command::Tray { .. }) => bail!("this build has no tray icon"),
        Some(Command::Worker { .. } | Command::DetectDevices) => unreachable!("handled above"),
    }
}

/// Runs the setup window or the terminal setup.
fn interactive_setup(cli: &Cli, paths: &Paths) -> anyhow::Result<Option<ClientConfig>> {
    #[cfg(feature = "gui")]
    if gui_available(cli) {
        return Ok(gui::run_setup(paths.clone()));
    }
    let _ = cli;
    setup::run_terminal(paths)
}

fn open_settings(cli: &Cli, paths: &Paths) -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    if gui_available(cli) {
        gui::run_settings(paths.clone());
        return Ok(());
    }
    let _ = cli;
    let config = ClientConfig::load(&paths.config_file).context("the client is not set up yet")?;
    println!("Configuration file: {}", paths.config_file.display());
    println!(
        "Check for updates every {} minutes",
        config.updates.check_interval_minutes
    );
    let yes_no = |on: bool| if on { "yes" } else { "no" };
    println!(
        "Install development builds: {}",
        yes_no(config.updates.include_dev_builds)
    );
    println!("Play tones: {}", yes_no(config.tones.enabled));
    println!("Play tones for transcription jobs: {}", yes_no(config.tones.jobs));
    println!(
        "Edit [updates] and [tones] in the file; a running client picks up changes with: verbarelay-client reload"
    );
    Ok(())
}

fn run_background(cli: &Cli, paths: &Paths) -> anyhow::Result<()> {
    updater::cleanup_old_files(&paths.install_dir);
    let info = BuildInfo::current();
    if cli.after_update {
        info!(version = %info.describe(), "updated");
    }

    // A second start opens the settings of the running client.
    if ipc::agent_running(paths) {
        return open_settings(cli, paths);
    }

    // After an update the restart tone already announced the new start.
    let mut greeted = cli.after_update;
    loop {
        let mut config = ClientConfig::load_optional(&paths.config_file)?;
        if config.as_ref().is_none_or(|c| c.credentials.is_none()) {
            config = interactive_setup(cli, paths)?;
        }
        let Some(config) = config else {
            info!("setup cancelled");
            return Ok(());
        };
        if let Err(e) = autostart::enable() {
            warn!("registering autostart failed: {e:#}");
        }

        info!(version = %info.describe(), server = %config.server_url, backend = %config.backend, "VerbaRelay client starting");
        tones::configure(&config.tones);
        if !std::mem::replace(&mut greeted, true) {
            tones::play(Tone::Startup);
        }
        match run_agent(paths, config, gui_available(cli))? {
            AgentExit::Restart => {
                tones::play_blocking(Tone::Restart);
                updater::restart()?;
                return Ok(());
            }
            AgentExit::Shutdown => {
                info!("stopping");
                tones::play_blocking(Tone::Shutdown);
                return Ok(());
            }
            AgentExit::NeedsEnrollment => {
                // Drop the stale token and set up again.
                if let Ok(Some(mut config)) = ClientConfig::load_optional(&paths.config_file) {
                    config.credentials = None;
                    config.save(&paths.config_file)?;
                }
                if !gui_available(cli) {
                    bail!("the server rejected this device (token expired or revoked); run `verbarelay-client setup`");
                }
            }
        }
    }
}

fn run_agent(paths: &Paths, config: ClientConfig, tray: bool) -> anyhow::Result<AgentExit> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async {
        let status: ipc::SharedStatus = Arc::new(Mutex::new(ipc::AgentStatus {
            version: BuildInfo::current().describe(),
            ..Default::default()
        }));
        let (commands_tx, commands_rx) = mpsc::channel(8);
        let (update_checks_tx, update_checks_rx) = mpsc::channel(4);
        let quit = CancellationToken::new();
        let controls = ipc::Controls {
            commands: commands_tx,
            update_checks: update_checks_tx,
            quit: quit.clone(),
        };
        let _endpoint = ipc::serve(paths, status.clone(), controls).await?;
        // The tray exits when this handle (its stdin) is dropped with the agent;
        // a restarted agent starts its own.
        let _tray = if tray { start_tray() } else { None };
        let (settings_tx, settings_rx) = watch::channel(config.updates.clone());
        let (staged_tx, staged_rx) = mpsc::channel(1);
        tokio::spawn(updater::run(
            settings_rx,
            paths.install_dir.clone(),
            paths.data_dir.clone(),
            staged_tx,
            update_checks_rx,
        ));
        let agent = Agent::new(paths.clone(), config, status, settings_tx, staged_rx, commands_rx)?;
        tokio::select! {
            exit = agent.run() => Ok(exit),
            () = stop_requested() => Ok(AgentExit::Shutdown),
            () = quit.cancelled() => Ok(AgentExit::Shutdown),
        }
    })
}

/// Starts the tray icon process. It is not killed but exits by itself once the
/// returned stdin closes, so it can remove its icon first.
fn start_tray() -> Option<std::process::ChildStdin> {
    let exe = std::env::current_exe().ok()?;
    std::process::Command::new(exe)
        .args(["tray", "--attached"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .inspect_err(|e| warn!("starting the tray icon failed: {e}"))
        .ok()?
        .stdin
        .take()
}

/// Resolves when the user or the system asks the client to stop.
async fn stop_requested() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_close, ctrl_logoff, ctrl_shutdown};
        let (Ok(mut close), Ok(mut logoff), Ok(mut shutdown)) = (ctrl_close(), ctrl_logoff(), ctrl_shutdown()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = close.recv() => {}
            _ = logoff.recv() => {}
            _ = shutdown.recv() => {}
        }
    }
}
