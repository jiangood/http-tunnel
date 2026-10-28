use crate::cli::{ClientArgs, DEFAULT_CLIENT_WEB_PORT};
use crate::client_env::{self, ClientEnv, LoadedConfig};
use crate::config::{ClientConfig, ClientTunnelConfig, MaskedString};
use crate::protocol_v4::Role;
use crate::session_client::{self, RouteMap, RouteTarget, SessionArgs};
use anyhow::{anyhow, bail, Result};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{broadcast, mpsc, watch, RwLock};
#[cfg(not(feature = "client-api"))]
use tracing::warn;
use tracing::{error, info};

/// The entrypoint of running a client.
pub async fn run_client(
    args: ClientArgs,
    mut shutdown_rx: broadcast::Receiver<bool>,
) -> Result<()> {
    // The fixed TOML file is the only configuration file, but every setting has
    // an `HTC_*` environment counterpart that wins over it. A missing or
    // unusable configuration is not fatal: it is reported and the process then
    // waits for a shutdown signal, so a container does not restart-loop while
    // its configuration is being fixed.
    let loaded = match client_env::load(&args.config_path).await {
        Ok(loaded) => loaded,
        Err(e) => return wait_after_error(e, &mut shutdown_rx).await,
    };

    // Log the sources, never the values
    if !loaded.env.is_empty() {
        info!(
            "Configuration overridden by the environment: {}",
            loaded.env.vars().join(", ")
        );
    }
    if loaded.env.tunnels_managed() {
        info!(
            "The tunnels are managed by HTC_TUNNELS; the web page and the API cannot change them"
        );
    }

    let identity = match resolve_identity(&loaded.effective, &args.config_path) {
        Ok(identity) => identity,
        Err(e) => return wait_after_error(e, &mut shutdown_rx).await,
    };
    let retry_interval = loaded.effective.retry_interval();
    let web_port = loaded.effective.web_port.unwrap_or(DEFAULT_CLIENT_WEB_PORT);
    #[cfg(feature = "client-api")]
    let web_host = loaded.effective.web_host().to_string();

    info!(
        "Using the WebSocket transport over {}: {} is the server's `server_host` and `server_port`",
        if identity.tls {
            "TLS (wss)"
        } else {
            "plain text (ws); do not use it on the public Internet"
        },
        identity.server_addr
    );

    // A definitive failure (a wrong token) is reported here so that the client
    // exits instead of retrying forever.
    let (fatal_tx, mut fatal_rx) = mpsc::unbounded_channel::<anyhow::Error>();
    let (routes_tx, routes_rx) = watch::channel(RouteMap::new());
    let connected = Arc::new(AtomicBool::new(false));
    let state = Arc::new(ClientState::new(
        loaded,
        args.config_path.clone(),
        identity,
        routes_tx,
        connected.clone(),
    ));

    // Publish the tunnels configured in the file.
    state.sync().await;

    // One v4 session carries every tunnel of this client.
    let session_args = SessionArgs {
        server_addr: state.server_addr().to_string(),
        tls: state.tls(),
        token: state.identity.token.clone(),
        node_id: state.name().to_string(),
        role: Role::Client,
        keepalive_interval: state.identity.keepalive_interval,
        retry_interval,
        routes: routes_rx,
        fatal_tx,
        connected,
    };
    let mut session_task =
        tokio::spawn(session_client::run(session_args, shutdown_rx.resubscribe()));

    // Run the web management interface and administration API when a port is
    // configured.
    #[cfg(feature = "client-api")]
    let web_task = match web_port {
        0 => None,
        port => {
            let token = state.token().to_owned();
            let state = state.clone();
            let shutdown_rx = shutdown_rx.resubscribe();
            Some(tokio::spawn(async move {
                if let Err(e) =
                    crate::client_api::serve(&web_host, port, token, state, shutdown_rx).await
                {
                    error!("{:#}", e);
                }
            }))
        }
    };

    #[cfg(not(feature = "client-api"))]
    if web_port != 0 {
        warn!(
            "The client web management interface is configured, but this build has \
             no `client-api` feature. It is disabled"
        );
    }

    let result = tokio::select! {
        err = fatal_rx.recv() => match err {
            Some(err) => Err(err),
            None => Ok(()),
        },
        task = &mut session_task => match task {
            Ok(result) => result,
            Err(err) => Err(anyhow!("The client session task failed: {}", err)),
        },
        _ = shutdown_rx.recv() => Ok(()),
    };

    // Withdraw the routes and close the session.
    state.shutdown().await;
    session_task.abort();
    let _ = session_task.await;

    #[cfg(feature = "client-api")]
    if let Some(web_task) = web_task {
        web_task.abort();
        let _ = web_task.await;
    }

    result
}

/// Resolve the identity of a client from its configuration, rejecting a missing
/// or placeholder value.
fn resolve_identity(config: &ClientConfig, config_path: &Path) -> Result<Identity> {
    let transport = config.transport_name().to_string();
    let server_addr = config.server_addr().ok_or_else(|| {
        anyhow!(
            "The server endpoint is missing. Set `server` in {:?} or `HTC_SERVER`, e.g. \
             `server = \"wss://control.example.com:443\"`. Its host must be the server's \
             `server_host`.",
            config_path
        )
    })?;
    let name = config.name.clone().ok_or_else(|| {
        anyhow!(
            "The name of the client is missing. Set `name` in {:?} or `HTC_NAME`",
            config_path
        )
    })?;
    let token = config.token.clone().ok_or_else(|| {
        anyhow!(
            "The token is missing. Set `token` in {:?} or `HTC_TOKEN`",
            config_path
        )
    })?;
    if &*token == "replace-with-the-same-secret-as-hts" {
        bail!(
            "Replace the placeholder token in {:?} or `HTC_TOKEN`",
            config_path
        );
    }
    let tls = config.tls();
    let keepalive_interval = config.keepalive_interval();
    Ok(Identity {
        server_addr,
        name,
        token,
        transport,
        tls,
        keepalive_interval,
    })
}

/// Report a startup error and then wait for a shutdown signal instead of
/// returning it.
async fn wait_after_error(
    e: anyhow::Error,
    shutdown_rx: &mut broadcast::Receiver<bool>,
) -> Result<()> {
    error!("{:#}", e);
    let _ = shutdown_rx.recv().await;
    Ok(())
}

/// The identity of a client, resolved from the configuration file and the
/// environment.
#[derive(Debug, Clone)]
pub(crate) struct Identity {
    pub(crate) server_addr: String,
    pub(crate) transport: String,
    pub(crate) name: String,
    pub(crate) token: MaskedString,
    /// Whether the WebSocket connection uses TLS (`wss://`)
    pub(crate) tls: bool,
    /// The WebSocket ping interval in secs, `0` meaning disabled
    pub(crate) keepalive_interval: u64,
}

/// The state of a client, shared between the session and the administration
/// API.
///
/// The configuration file is the persisted source of truth: a change made
/// through the API is written back to it and published to the session. The
/// `HTC_*` environment variables override it at runtime, but are never written
/// back.
pub(crate) struct ClientState {
    config: RwLock<ClientConfig>,
    persisted: RwLock<ClientConfig>,
    env: ClientEnv,
    config_path: PathBuf,
    identity: Identity,
    /// The routes the session advertises. The session reads this watch and
    /// sends diffs to the next hop.
    routes: watch::Sender<RouteMap>,
    /// Whether the session is established, reported by the administration API.
    connected: Arc<AtomicBool>,
}

impl ClientState {
    fn new(
        loaded: LoadedConfig,
        config_path: PathBuf,
        identity: Identity,
        routes: watch::Sender<RouteMap>,
        connected: Arc<AtomicBool>,
    ) -> ClientState {
        ClientState {
            config: RwLock::new(loaded.effective),
            persisted: RwLock::new(loaded.persisted),
            env: loaded.env,
            config_path,
            identity,
            routes,
            connected,
        }
    }

    /// The shared token, used by the administration API for authentication
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn token(&self) -> &str {
        &self.identity.token
    }

    /// The name of the client, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn name(&self) -> &str {
        &self.identity.name
    }

    /// The address of the server, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn server_addr(&self) -> &str {
        &self.identity.server_addr
    }

    /// The transport mode, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn transport(&self) -> &str {
        &self.identity.transport
    }

    /// Whether the session uses TLS
    fn tls(&self) -> bool {
        self.identity.tls
    }

    /// Whether the session is established, reported by the administration API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether the tunnels come from `HTC_TUNNELS` and thus cannot be changed
    /// through the API
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) fn tunnels_managed(&self) -> bool {
        self.env.tunnels_managed()
    }

    /// The tunnels the client is serving, sorted by name
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn tunnels(&self) -> Vec<ClientTunnelConfig> {
        self.config.read().await.resolved_tunnels()
    }

    /// Publish the tunnels as the routes of the session.
    async fn sync(&self) {
        let tunnels = self.config.read().await.resolved_tunnels();
        let routes: RouteMap = tunnels
            .into_iter()
            .map(|tunnel| {
                (
                    tunnel.domain,
                    RouteTarget {
                        local_addr: tunnel.local_addr,
                        auth_header: tunnel.auth_header,
                    },
                )
            })
            .collect();
        self.routes.send_replace(routes);
    }

    /// Apply a change to the persisted configuration, save it, and recompute the
    /// effective configuration with the environment overlay.
    ///
    /// The file is written first, so that a failed write leaves the runtime
    /// untouched. The environment values are reapplied afterwards: they win at
    /// runtime but must not end up in the file.
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    async fn update_persisted<F>(&self, change: F) -> Result<()>
    where
        F: FnOnce(&mut ClientConfig) -> Result<()>,
    {
        if self.env.tunnels_managed() {
            bail!(
                "The tunnels are managed by HTC_TUNNELS; change that environment variable and \
                 restart htc"
            );
        }

        let mut persisted = self.persisted.write().await;
        let mut candidate = persisted.clone();
        change(&mut candidate)?;
        // Validate after the change, so that a name or domain clash is rejected
        // before anything is written
        ClientConfig::validate(&mut candidate)?;
        candidate.save(&self.config_path).await?;

        let mut effective = candidate.clone();
        self.env.apply(&mut effective);
        ClientConfig::validate(&mut effective)?;

        *persisted = candidate;
        *self.config.write().await = effective;
        Ok(())
    }

    /// Create or replace the tunnel of the given name, persist the
    /// configuration, and apply the change
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn put_tunnel(
        &self,
        name: String,
        domain: String,
        local_addr: String,
    ) -> Result<()> {
        if local_addr.is_empty() {
            bail!("The `local_addr` must not be empty");
        }
        let domain = crate::helper::normalize_domain(&domain)
            .ok_or_else(|| anyhow!("The domain `{}` is invalid", domain))?;

        self.update_persisted(|config| {
            config.set_tunnel(name, domain, local_addr);
            Ok(())
        })
        .await?;

        self.sync().await;
        Ok(())
    }

    /// Delete the tunnel of the given name, persist the configuration, and
    /// apply the change
    #[cfg_attr(not(feature = "client-api"), allow(dead_code))]
    pub(crate) async fn delete_tunnel(&self, name: &str) -> Result<()> {
        self.update_persisted(|config| {
            if !config.remove_tunnel(name.trim()) {
                bail!("No such a tunnel `{}`", name);
            }
            Ok(())
        })
        .await?;

        self.sync().await;
        Ok(())
    }

    /// Withdraw every route.
    async fn shutdown(&self) {
        self.routes.send_replace(RouteMap::new());
        self.connected.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client_env::{ENV_TOKEN, ENV_TUNNELS};

    fn identity() -> Identity {
        Identity {
            server_addr: "127.0.0.1:8080".to_string(),
            transport: "ws".to_string(),
            name: "test".to_string(),
            token: MaskedString::from("token"),
            tls: false,
            keepalive_interval: 15,
        }
    }

    fn state(loaded: LoadedConfig, path: PathBuf) -> ClientState {
        let (routes, _) = watch::channel(RouteMap::new());
        ClientState::new(
            loaded,
            path,
            identity(),
            routes,
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[tokio::test]
    async fn test_api_edits_persist_only_the_file_baseline() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-client-{}-baseline.toml",
            std::process::id()
        ));
        let persisted = ClientConfig::parse(
            r#"
server = "wss://file.example.com:443"
name = "file"
token = "file-token"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        let env = ClientEnv::from_lookup(|key| {
            Ok(match key {
                ENV_TOKEN => Some("env-token".to_string()),
                _ => None,
            })
        })?;
        let mut effective = persisted.clone();
        env.apply(&mut effective);
        let state = state(
            LoadedConfig {
                effective,
                persisted,
                env,
            },
            path.clone(),
        );

        state
            .update_persisted(|config| {
                config.set_tunnel(
                    "git".to_string(),
                    "git.example.com".to_string(),
                    "127.0.0.1:81".to_string(),
                );
                Ok(())
            })
            .await?;

        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);

        // The environment token wins at runtime but is never written to disk
        assert!(saved.contains("file-token"), "{}", saved);
        assert!(!saved.contains("env-token"), "{}", saved);
        // The new tunnel is persisted
        assert!(saved.contains("git.example.com"), "{}", saved);
        assert_eq!(state.tunnels().await.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_environment_tunnels_are_read_only() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-client-{}-read-only.toml",
            std::process::id()
        ));
        let env = ClientEnv::from_lookup(|key| {
            Ok(match key {
                ENV_TUNNELS => Some(
                    r#"[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]"#
                        .to_string(),
                ),
                _ => None,
            })
        })?;
        let mut effective = ClientConfig::default();
        env.apply(&mut effective);
        ClientConfig::validate(&mut effective)?;
        let state = state(
            LoadedConfig {
                effective,
                persisted: ClientConfig::default(),
                env,
            },
            path.clone(),
        );

        assert!(state.tunnels_managed());
        let err = state
            .put_tunnel(
                "git".to_string(),
                "git.example.com".to_string(),
                "127.0.0.1:81".to_string(),
            )
            .await
            .expect_err("the tunnels of the environment must not be changed");
        assert!(format!("{:#}", err).contains("HTC_TUNNELS"), "{}", err);
        assert!(!path.exists(), "no configuration file must be created");
        Ok(())
    }
}
