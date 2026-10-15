use anyhow::Result;
use clap::Parser;
use http_tunnel::{run, Command, ServerArgs, ServerConfig};
use std::path::Path;
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[clap(name = "hts", version, about = "HTTP tunnel server")]
struct Cli {}

#[tokio::main]
async fn main() -> Result<()> {
    Cli::parse();
    let config = ServerConfig::from_file(Path::new("hts.toml")).await?;
    let cmd = Command::Server(ServerArgs {
        server_port: config.server_port,
        http_port: config.http_port,
        token: String::from(&*config.token),
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
