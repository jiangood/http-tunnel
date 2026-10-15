use anyhow::{bail, Result};
use clap::Parser;
use http_tunnel::{run, Cli};
use std::ffi::OsString;
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Keep the CLI ergonomic: hts accepts server options directly, without a
    // redundant "server" subcommand.
    let mut args = std::env::args_os();
    let program = args.next().unwrap_or_else(|| OsString::from("hts"));
    let cli = Cli::try_parse_from(
        std::iter::once(program)
            .chain(std::iter::once(OsString::from("server")))
            .chain(args),
    )?;
    let cmd = cli.into_command()?;
    if !matches!(cmd, http_tunnel::Command::Server(_)) {
        bail!("hts only runs the server");
    }

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    let is_atty = atty::is(atty::Stream::Stdout);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::from("info")))
        .with_ansi(is_atty)
        .init();

    run(cmd, shutdown_rx).await
}
