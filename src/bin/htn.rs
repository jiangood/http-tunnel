use anyhow::Result;
use clap::Parser;
use http_tunnel::{run, Command, NodeArgs, NodeConfig};
use std::{
    fs::OpenOptions,
    path::Path,
    process::{Command as ProcessCommand, Stdio},
};
use tokio::{signal, sync::broadcast};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[clap(
    name = "htn",
    version,
    about = "HTTP tunnel node: one process per relay hop"
)]
struct Cli {
    /// Run in the background and write output to htn.log
    #[clap(short = 'b', long)]
    background: bool,
    /// Internal flag used by the background launcher
    #[clap(long, hide = true)]
    background_child: bool,
}

fn launch_background() -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open("htn.log")?;
    let err_log = log.try_clone()?;
    let mut command = ProcessCommand::new(exe);
    command
        .arg("--background-child")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let child = command.spawn()?;
    println!(
        "Started htn in background (pid {}). Log: htn.log",
        child.id()
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.background && !cli.background_child {
        return launch_background();
    }
    let config = NodeConfig::from_file(Path::new("htn.toml")).await?;

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

    run(Command::Node(NodeArgs { config }), shutdown_rx).await
}
