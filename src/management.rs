//! Management state shared by the universal process and its web API.
//!
//! The API manages the local `[[proxies]]` of the same configuration that also
//! drives the embedded listener. A change is persisted to `ht.toml`, validated,
//! applied to the running server's proxy list, and published on the route watch
//! so an upstream session mirrors it immediately.

use crate::config::{Config, ConfigSource, ServerProxyEntry};
use crate::config_env::EnvConfig;
use crate::protocol_v4;
use crate::server::{ServerState, SessionLookupError, SessionTarget};
use crate::session_server::SessionControl;
use anyhow::{anyhow, bail, Result};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, RwLock};
use tokio::time;
use tracing::info;

/// The local services of one downstream node, as returned by its session.
pub(crate) struct SessionProxySnapshot {
    pub(crate) writable: bool,
    pub(crate) proxies: Vec<ServerProxyEntry>,
}

/// Why a management request aimed at a downstream session failed.
pub(crate) enum SessionAdminError {
    NotFound(String),
    Ambiguous(Vec<u64>),
    BadRequest(String),
    NotManageable,
    ReadOnly,
    Timeout,
    Downstream(String),
}

impl fmt::Display for SessionAdminError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionAdminError::NotFound(message) => write!(f, "{}", message),
            SessionAdminError::Ambiguous(ids) => write!(
                f,
                "Several connected sessions use this node name; use the `session` parameter \
                 with one of: {}",
                ids.iter()
                    .map(|id| id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            SessionAdminError::BadRequest(message) => write!(f, "{}", message),
            SessionAdminError::NotManageable => write!(
                f,
                "The downstream node does not support remote proxy management"
            ),
            SessionAdminError::ReadOnly => write!(
                f,
                "The downstream node manages its local services with HT_PROXIES; change that \
                 variable on the node and restart it"
            ),
            SessionAdminError::Timeout => write!(
                f,
                "The downstream node did not answer the management request in time"
            ),
            SessionAdminError::Downstream(message) => write!(f, "{}", message),
        }
    }
}

impl fmt::Debug for SessionAdminError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for SessionAdminError {}

/// The state behind the web management page and administration API.
pub(crate) struct ManagementState {
    config: RwLock<Config>,
    persisted: RwLock<Config>,
    env: EnvConfig,
    config_path: PathBuf,
    server: Arc<ServerState>,
    /// Whether the upstream session is established. A standalone ingress has
    /// no upstream and is always considered reachable.
    connected: Arc<AtomicBool>,
    name: String,
    server_addr: String,
    transport: String,
    /// When the management state was created, for the uptime of the console
    started: Instant,
}

impl ManagementState {
    pub(crate) fn new(
        config: Config,
        server: Arc<ServerState>,
        connected: Arc<AtomicBool>,
    ) -> ManagementState {
        let source = &config.source;
        let config_path = source
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("ht.toml"));
        let env = source.env.clone();
        let mut persisted = source
            .persisted
            .as_deref()
            .cloned()
            .unwrap_or_else(|| config.clone());
        persisted.source = ConfigSource::default();

        let name = config.name.clone();
        let server_addr = config.upstream_addr().unwrap_or_else(|| {
            crate::helper::to_bind_addr(crate::helper::DEFAULT_BIND_HOST, config.listen_port)
        });
        let transport = if config.upstream.is_some() {
            config.transport_name().to_string()
        } else {
            "ingress".to_string()
        };

        ManagementState {
            config: RwLock::new(config),
            persisted: RwLock::new(persisted),
            env,
            config_path,
            server,
            connected,
            name,
            server_addr,
            transport,
            started: Instant::now(),
        }
    }

    /// The name of the process, reported by the administration API
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// The upstream address, or the listener address of a standalone ingress
    pub(crate) fn server_addr(&self) -> &str {
        &self.server_addr
    }

    /// The transport mode, reported by the administration API
    pub(crate) fn transport(&self) -> &str {
        &self.transport
    }

    /// Whether the upstream session is established
    pub(crate) fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Whether the proxies come from `HT_PROXIES` and thus cannot be changed
    /// through the API
    pub(crate) fn proxies_managed(&self) -> bool {
        self.env.proxies_managed()
    }

    /// The configuration file the API writes back to
    pub(crate) fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Seconds since the process started, shown by the management page
    pub(crate) fn uptime(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// The `HT_*` variables the environment sets, with their values omitted
    pub(crate) fn env_vars(&self) -> Vec<(&'static str, bool)> {
        self.env.describe()
    }

    /// The local services the process is serving, sorted by name
    pub(crate) async fn proxies(&self) -> Vec<ServerProxyEntry> {
        let mut proxies = self.config.read().await.proxies.clone();
        proxies.sort_by(|a, b| a.name.cmp(&b.name));
        proxies
    }

    /// The effective routing table: local services plus the routes claimed by
    /// v4 sessions, as seen by `GET /api/routes`.
    pub(crate) async fn routes(&self) -> Vec<crate::server::RouteSnapshot> {
        self.server.routes_snapshot().await
    }

    /// The downstream v4 sessions connected right now, as seen by
    /// `GET /api/sessions`.
    pub(crate) async fn sessions(&self) -> Vec<crate::server::SessionSnapshot> {
        self.server.sessions_snapshot().await
    }

    /// The number of connected v4 sessions and the configured limit.
    pub(crate) fn session_count(&self) -> (usize, usize) {
        self.server.session_count()
    }

    /// Resolve a downstream target from the optional `node` and `session`
    /// selectors. Both may be given; they must then identify the same session.
    async fn resolve_session(
        &self,
        node: Option<&str>,
        session: Option<u64>,
    ) -> std::result::Result<SessionTarget, SessionAdminError> {
        match (node, session) {
            (Some(node), Some(session_id)) => {
                let target = self
                    .server
                    .session_target_by_id(session_id)
                    .await
                    .ok_or_else(|| {
                        SessionAdminError::NotFound(format!(
                            "No such downstream session: {}",
                            session_id
                        ))
                    })?;
                if target.node_id != node {
                    return Err(SessionAdminError::BadRequest(format!(
                        "The session {} belongs to node `{}`, not `{}`",
                        session_id, target.node_id, node
                    )));
                }
                Ok(target)
            }
            (Some(node), None) => self
                .server
                .session_target_by_node(node)
                .await
                .map_err(|error| match error {
                    SessionLookupError::NotFound => {
                        SessionAdminError::NotFound(format!("No such downstream node: {}", node))
                    }
                    SessionLookupError::Ambiguous(ids) => SessionAdminError::Ambiguous(ids),
                }),
            (None, Some(session_id)) => self
                .server
                .session_target_by_id(session_id)
                .await
                .ok_or_else(|| {
                    SessionAdminError::NotFound(format!(
                        "No such downstream session: {}",
                        session_id
                    ))
                }),
            (None, None) => Err(SessionAdminError::BadRequest(
                "A downstream proxy request needs `node` or `session`".to_string(),
            )),
        }
    }

    /// Send one proxy operation to a downstream session and wait for its answer.
    async fn request_session(
        &self,
        target: &SessionTarget,
        op: protocol_v4::ProxyOp,
    ) -> std::result::Result<Vec<ServerProxyEntry>, SessionAdminError> {
        if target.capabilities & protocol_v4::CAP_PROXY_ADMIN == 0 {
            return Err(SessionAdminError::NotManageable);
        }
        if !matches!(op, protocol_v4::ProxyOp::List)
            && target.capabilities & protocol_v4::CAP_PROXY_WRITE == 0
        {
            return Err(SessionAdminError::ReadOnly);
        }
        let (reply, reply_rx) = oneshot::channel();
        match time::timeout(
            Duration::from_secs(5),
            target.control.send(SessionControl::Proxy { op, reply }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(SessionAdminError::NotFound(
                    "The downstream session closed before the request".to_string(),
                ));
            }
            Err(_) => return Err(SessionAdminError::Timeout),
        }
        match time::timeout(Duration::from_secs(10), reply_rx).await {
            Ok(Ok(Ok(proxies))) => Ok(proxies
                .into_iter()
                .map(|proxy| ServerProxyEntry {
                    name: proxy.name,
                    domain: proxy.domain,
                    local_addr: proxy.local_addr,
                    auth_header: None,
                })
                .collect()),
            Ok(Ok(Err(error))) => {
                // The downstream uses the same wording as the local API for a
                // missing proxy. Keep that a `404` instead of a generic `502`.
                if error.contains("No such") {
                    Err(SessionAdminError::NotFound(error))
                } else {
                    Err(SessionAdminError::Downstream(error))
                }
            }
            Ok(Err(_)) => Err(SessionAdminError::Downstream(
                "The downstream session dropped the management request".to_string(),
            )),
            Err(_) => Err(SessionAdminError::Timeout),
        }
    }

    /// The local services of one downstream node.
    pub(crate) async fn session_proxies(
        &self,
        node: Option<&str>,
        session: Option<u64>,
    ) -> std::result::Result<SessionProxySnapshot, SessionAdminError> {
        let target = self.resolve_session(node, session).await?;
        let proxies = self
            .request_session(&target, protocol_v4::ProxyOp::List)
            .await?;
        Ok(session_snapshot(&target, proxies))
    }

    /// Create or replace one local service on a downstream node.
    pub(crate) async fn put_session_proxy(
        &self,
        node: Option<&str>,
        session: Option<u64>,
        name: String,
        domain: String,
        local_addr: String,
    ) -> std::result::Result<SessionProxySnapshot, SessionAdminError> {
        let name = crate::config::validate_name(&name)
            .map_err(|error| SessionAdminError::BadRequest(format!("{:#}", error)))?;
        let domain = crate::helper::normalize_domain(&domain).ok_or_else(|| {
            SessionAdminError::BadRequest(format!("The domain `{}` is invalid", domain))
        })?;
        let local_addr = validate_local_addr(&local_addr)?;
        let target = self.resolve_session(node, session).await?;
        let proxies = self
            .request_session(
                &target,
                protocol_v4::ProxyOp::Put {
                    name,
                    domain,
                    local_addr,
                },
            )
            .await?;
        Ok(session_snapshot(&target, proxies))
    }

    /// Delete one local service on a downstream node.
    pub(crate) async fn delete_session_proxy(
        &self,
        node: Option<&str>,
        session: Option<u64>,
        name: &str,
    ) -> std::result::Result<SessionProxySnapshot, SessionAdminError> {
        let name = crate::config::validate_name(name)
            .map_err(|error| SessionAdminError::BadRequest(format!("{:#}", error)))?;
        let target = self.resolve_session(node, session).await?;
        let proxies = self
            .request_session(&target, protocol_v4::ProxyOp::Delete { name })
            .await?;
        Ok(session_snapshot(&target, proxies))
    }

    /// Close one downstream session. The node may reconnect by itself.
    pub(crate) async fn disconnect_session(
        &self,
        session_id: u64,
    ) -> std::result::Result<(), SessionAdminError> {
        let target = self
            .server
            .session_target_by_id(session_id)
            .await
            .ok_or_else(|| {
                SessionAdminError::NotFound(format!("No such downstream session: {}", session_id))
            })?;
        let (reply, reply_rx) = oneshot::channel();
        match time::timeout(
            Duration::from_secs(5),
            target.control.send(SessionControl::Close { reply }),
        )
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(SessionAdminError::NotFound(
                    "The downstream session closed before the request".to_string(),
                ));
            }
            Err(_) => return Err(SessionAdminError::Timeout),
        }
        match time::timeout(Duration::from_secs(5), reply_rx).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(SessionAdminError::Downstream(
                "The downstream session did not confirm the close".to_string(),
            )),
            Err(_) => Err(SessionAdminError::Timeout),
        }
    }

    /// Apply a change to the persisted configuration, save it, and recompute the
    /// effective configuration with the environment overlay.
    ///
    /// The file is written first, so that a failed write leaves the runtime
    /// untouched. The environment values are reapplied afterwards: they win at
    /// runtime but must not end up in the file.
    async fn update_persisted<F>(&self, change: F) -> Result<()>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        if self.env.proxies_managed() {
            bail!(
                "The local services are managed by HT_PROXIES; change that environment \
                 variable and restart ht"
            );
        }

        let mut persisted = self.persisted.write().await;
        let mut candidate = persisted.clone();
        change(&mut candidate)?;
        // Validate after the change, so that a name or domain clash is rejected
        // before anything is written
        Config::validate(&mut candidate)?;
        candidate.save(&self.config_path).await?;

        let mut effective = candidate.clone();
        self.env.apply(&mut effective);
        Config::validate(&mut effective)?;
        effective.source = ConfigSource {
            path: Some(self.config_path.clone()),
            persisted: Some(Box::new(candidate.clone())),
            env: self.env.clone(),
            env_only: false,
        };

        *persisted = candidate;
        *self.config.write().await = effective.clone();
        // Publish the new proxy list before returning, so that the next visitor
        // and the upstream route watch see it.
        let proxy_count = effective.proxies.len();
        self.server.set_proxies(effective.proxies).await;
        info!(proxies = proxy_count, "The local services were updated");
        Ok(())
    }

    /// Create or replace the local service of the given name, persist the
    /// configuration, and apply the change.
    pub(crate) async fn put_proxy(
        &self,
        name: String,
        domain: String,
        local_addr: String,
    ) -> Result<()> {
        if local_addr.trim().is_empty() {
            bail!("The `local_addr` must not be empty");
        }
        let domain = crate::helper::normalize_domain(&domain)
            .ok_or_else(|| anyhow!("The domain `{}` is invalid", domain))?;

        self.update_persisted(|config| {
            config.set_proxy(name, domain, local_addr);
            Ok(())
        })
        .await
    }

    /// Delete the local service of the given name, persist the configuration,
    /// and apply the change.
    pub(crate) async fn delete_proxy(&self, name: &str) -> Result<()> {
        self.update_persisted(|config| {
            if !config.remove_proxy(name.trim()) {
                bail!("No such a local service `{}`", name);
            }
            Ok(())
        })
        .await
    }
}

fn session_snapshot(
    target: &SessionTarget,
    proxies: Vec<ServerProxyEntry>,
) -> SessionProxySnapshot {
    SessionProxySnapshot {
        writable: target.capabilities & protocol_v4::CAP_PROXY_WRITE != 0,
        proxies,
    }
}

/// Validate the address shape before asking the downstream node, so a URL or an
/// empty target is a local `400` instead of a round trip and a `502`.
fn validate_local_addr(local_addr: &str) -> std::result::Result<String, SessionAdminError> {
    let local_addr = local_addr.trim();
    if local_addr.is_empty() {
        return Err(SessionAdminError::BadRequest(
            "The `local_addr` must not be empty".to_string(),
        ));
    }
    if let Some((scheme, _)) = local_addr.split_once("://") {
        if scheme != "unix" {
            return Err(SessionAdminError::BadRequest(format!(
                "The `local_addr` must be `host:port` or `unix:///path`, not a URL: \
                 remove the `{}://` from `{}`",
                scheme, local_addr
            )));
        }
    }
    Ok(local_addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MaskedString;
    use crate::server::ServerState;
    use std::collections::BTreeSet;
    use tokio::sync::watch;

    fn state(loaded: Config, path: PathBuf) -> ManagementState {
        let (routes, _) = watch::channel(BTreeSet::new());
        let config = {
            let mut config = loaded;
            config.source.path = Some(path);
            config.source.env_only = false;
            if config.source.persisted.is_none() {
                let mut baseline = config.clone();
                baseline.source = ConfigSource::default();
                config.source.persisted = Some(Box::new(baseline));
            }
            config
        };
        let server = Arc::new(ServerState::new(
            MaskedString::from("token"),
            config.name.clone(),
            "control.test".to_string(),
            config.server_limits(),
            routes,
            config.proxies.clone(),
        ));
        ManagementState::new(config, server, Arc::new(AtomicBool::new(true)))
    }

    #[tokio::test]
    async fn test_api_edits_persist_only_the_file_baseline() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-baseline.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            r#"
control_host = "control.example.com"
name = "file"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        let env = EnvConfig::from_lookup(|key| {
            Ok(match key {
                crate::config_env::ENV_TOKEN => Some("env-token".to_string()),
                _ => None,
            })
        })?;
        let mut effective = persisted.clone();
        env.apply(&mut effective);
        Config::validate(&mut effective)?;
        effective.source.env = env;
        effective.source.persisted = Some(Box::new(persisted.clone()));
        let state = state(effective, path.clone());

        state
            .put_proxy(
                "git".to_string(),
                "git.example.com".to_string(),
                "127.0.0.1:81".to_string(),
            )
            .await?;

        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);

        // The environment token wins at runtime but is never written to disk
        assert!(saved.contains("file-token"), "{}", saved);
        assert!(!saved.contains("env-token"), "{}", saved);
        // The new local service is persisted under [[proxies]]
        assert!(saved.contains("git.example.com"), "{}", saved);
        assert!(saved.contains("[[proxies]]"), "{}", saved);
        assert_eq!(state.proxies().await.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_environment_proxies_are_read_only() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-read-only.toml",
            std::process::id()
        ));
        let env = EnvConfig::from_lookup(|key| {
            Ok(match key {
                crate::config_env::ENV_PROXIES => Some(
                    r#"[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]"#
                        .to_string(),
                ),
                _ => None,
            })
        })?;
        let mut effective = Config::default();
        env.apply(&mut effective);
        effective.control_host = Some("control.example.com".to_string());
        effective.token = Some(MaskedString::from("secret"));
        Config::validate(&mut effective)?;
        effective.source.env = env;
        let state = state(effective, path.clone());

        assert!(state.proxies_managed());
        let err = state
            .put_proxy(
                "git".to_string(),
                "git.example.com".to_string(),
                "127.0.0.1:81".to_string(),
            )
            .await
            .expect_err("the proxies of the environment must not be changed");
        assert!(format!("{:#}", err).contains("HT_PROXIES"), "{}", err);
        assert!(!path.exists(), "no configuration file must be created");
        Ok(())
    }

    #[tokio::test]
    async fn test_url_local_addr_is_rejected_before_persisting() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-url-local-addr.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            r#"
control_host = "control.example.com"
name = "file"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        std::fs::write(&path, persisted.to_toml()?)?;
        let state = state(persisted, path.clone());

        let err = state
            .put_proxy(
                "git".to_string(),
                "git.example.com".to_string(),
                "http://127.0.0.1:81".to_string(),
            )
            .await
            .expect_err("a URL must not be accepted as a local address");
        let message = format!("{:#}", err);
        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);

        assert!(
            message.contains("must be `host:port` or `unix:///path`"),
            "{}",
            message
        );
        assert!(!saved.contains("http://127.0.0.1:81"), "{}", saved);
        assert_eq!(state.proxies().await.len(), 1);
        Ok(())
    }
}
