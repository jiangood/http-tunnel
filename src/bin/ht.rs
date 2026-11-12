use anyhow::{Context, Result};
use clap::Parser;
use http_tunnel::{load_config, run};
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
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

/// Detect a first start: the configuration file is missing or carries no
/// configuration (only whitespace).
///
/// An empty file is removed so `load_config` takes the missing-file path and
/// regenerates the default; only whitespace is discarded, so no configuration
/// is ever lost. The hot reload does not get this treatment: there a
/// temporarily empty or half-written file is ignored while the running
/// configuration stays in place.
async fn first_start(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }
    match tokio::fs::read_to_string(path).await {
        Ok(content) if content.trim().is_empty() => {
            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("Failed to remove the empty config {:?}", path))?;
            Ok(true)
        }
        // Unreadable or non-UTF-8 content keeps the old behavior: `load_config`
        // reports the same failure it did before this check existed.
        _ => Ok(false),
    }
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
    // A missing or empty file is a first start, not a failure: `load_config`
    // writes the commented example and the process keeps running with it.
    let generated = first_start(&cli.config).await?;
    let config = load_config(&cli.config).await?;
    if generated {
        match config.entry_domain() {
            // A detected local IP enables the control plane out of the box.
            Some(domain) => println!(
                "未找到 {:?} 或其内容为空，已生成默认配置并以服务端模式启动。\
                 管理页: http://{}:{} ，用户名 ht，密码为 [server].token（见 {:?}）；\
                 保存配置后 `ht` 会自动热加载。",
                cli.config,
                domain,
                config.server.as_ref().map(|s| s.port).unwrap_or_default(),
                cli.config
            ),
            None => println!(
                "未找到 {:?} 或其内容为空，已生成默认配置并以服务端模式启动。\
                 需要管理页和下游 v4 入口时，请为 [server] 填写 domain 和 token；\
                 `ht` 会自动热加载保存后的配置。",
                cli.config
            ),
        }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ht-first-start-{}-{}", name, std::process::id()))
    }

    #[tokio::test]
    async fn missing_file_is_a_first_start() -> Result<()> {
        let path = temp_dir("missing").join("ht.toml");
        let _ = std::fs::remove_dir_all(temp_dir("missing"));

        assert!(first_start(&path).await?);
        assert!(!path.exists(), "nothing must be written before load_config");

        let _ = std::fs::remove_dir_all(temp_dir("missing"));
        Ok(())
    }

    #[tokio::test]
    async fn empty_file_is_a_first_start_and_is_regenerated() -> Result<()> {
        let dir = temp_dir("empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;

        // An empty file carries no configuration: it is dropped so the
        // default config is generated instead of failing to parse.
        for content in ["", "  \r\n\t ", "\n\n"] {
            let path = dir.join("ht.toml");
            std::fs::write(&path, content)?;
            assert!(first_start(&path).await?);
            assert!(!path.exists(), "the empty file must be removed");
        }

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn configured_file_is_not_a_first_start() -> Result<()> {
        let dir = temp_dir("configured");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("ht.toml");
        std::fs::write(&path, "[server]\nport = 7777\n")?;

        assert!(!first_start(&path).await?);
        assert!(
            path.exists(),
            "a file with configuration must be kept as-is"
        );

        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
