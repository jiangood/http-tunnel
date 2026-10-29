use anyhow::Result;
use clap::Parser;
use http_tunnel::{load_config, run, GeneratedConfigError};
use std::{
    fs::OpenOptions,
    path::PathBuf,
    process::{Command as ProcessCommand, Stdio},
};
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[clap(
    name = "ht",
    version,
    about = "HTTP tunnel: a universal router, relay and ingress in one process"
)]
struct Cli {
    /// Read this configuration file
    #[clap(short = 'c', long, default_value = "ht.toml")]
    config: PathBuf,
    /// Run in the background and write output to ht.log
    #[clap(short = 'b', long)]
    background: bool,
    /// Internal flag used by the background launcher
    #[clap(long, hide = true)]
    background_child: bool,
}

fn launch_background(config: &PathBuf) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("ht.log")?;
    let err_log = log.try_clone()?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("--background-child")
        .arg("--config")
        .arg(config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    println!("Started ht in background (pid {}). Log: ht.log", child.id());
    Ok(())
}

/// Wait for the signals a service manager sends. Docker and systemd stop a
/// process with `SIGTERM`, while an interactive terminal uses `SIGINT`; both
/// must start the same graceful shutdown.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal as unix_signal, SignalKind};
        match unix_signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(err) => {
                tracing::warn!("Failed to listen for SIGTERM: {:#}", err);
                let _ = signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.background && !cli.background_child {
        return launch_background(&cli.config);
    }
    let config = match load_config(&cli.config).await {
        Ok(config) => config,
        // Writing the example is the expected first run, not a failure.
        Err(err) if err.downcast_ref::<GeneratedConfigError>().is_some() => {
            println!("{err:#}");
            return Ok(());
        }
        Err(err) => return Err(err),
    };

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(true);
    });

    let is_atty = atty::is(atty::Stream::Stdout);
    // `RUST_LOG` lets an operator raise the level (for example `RUST_LOG=debug`
    // or `RUST_LOG=http_tunnel=debug`) without rebuilding, which is what the
    // per-request routing logs need to be visible.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(is_atty)
        .init();

    run(config, shutdown_rx).await
}
