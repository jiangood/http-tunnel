use anyhow::Result;
use clap::Parser;
use http_tunnel::{run, ClientArgs, Command};
use std::path::PathBuf;
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[clap(name = "htc", version, about = "HTTP tunnel client")]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    Cli::parse();
    let cmd = Command::Client(ClientArgs {
        config_path: PathBuf::from("htc.toml"),
    });

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<bool>(1);
    tokio::spawn(async move {
        if signal::ctrl_c().await.is_ok() {
            let _ = shutdown_tx.send(true);
        }
    });

    let is_atty = atty::is(atty::Stream::Stdout);
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from("info"))
        .with_ansi(is_atty)
        .init();

    run(cmd, shutdown_rx).await
}
