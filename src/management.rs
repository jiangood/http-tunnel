//! Management state shared by the universal process and its web API.
//!
//! The API manages the local `[[proxies]]` of the same configuration that also
//! drives the embedded listener. A change is persisted to `ht.toml`, validated,
//! applied to the running server's proxy list, and published on the route watch
//! so an upstream session mirrors it immediately.

use crate::config::{Config, ConfigSource, MaskedString, ServerProxyEntry};
use crate::protocol_v4;
use crate::reload::ReloadMarker;
use crate::server::{ServerState, SessionLookupError, SessionTarget};
use crate::session_server::SessionControl;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch, Mutex, RwLock};
use tokio::time;
use tracing::info;

/// How long a local service probe may take before it is reported as a timeout.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// The result of changing this node's configuration.
pub(crate) struct SettingsOutcome {
    /// The configuration after the change
    pub(crate) config: Config,
    /// Whether the process still has to rebuild its listener or upstream
    /// session before the new configuration is active
    pub(crate) restart_required: bool,
}

/// The result of dialling one local service from this node's perspective.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct ProbeOutcome {
    pub(crate) name: String,
    pub(crate) local_addr: String,
    pub(crate) ok: bool,
    pub(crate) latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<String>,
}

/// The local services of one downstream node, as returned by its session.
pub(crate) struct SessionProxySnapshot {
    /// `file` for a current downstream node, `env` for a legacy peer that
    /// still reported environment-managed proxies.
    pub(crate) proxy_source: &'static str,
    pub(crate) proxies: Vec<ServerProxyEntry>,
}

/// Why a management request aimed at a downstream session failed.
pub(crate) enum SessionAdminError {
    NotFound(String),
    Ambiguous(Vec<u64>),
    BadRequest(String),
    NotManageable,
    ReadOnly,
    /// The downstream node predates the `auth_header` wire shape
    Unsupported(String),
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
                "The downstream node does not allow its upstream to modify its local services: \
                 set `[upstream].allow_modify = true` on that node and restart it"
            ),
            SessionAdminError::Unsupported(message) => write!(f, "{}", message),
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
    /// Serializes read-modify-write configuration changes from the API, so two
    /// concurrent requests cannot overwrite each other.
    updates: Mutex<()>,
    config_path: PathBuf,
    server: Arc<ServerState>,
    /// Whether the upstream session is established. A standalone ingress has
    /// no upstream and is always considered reachable.
    connected: Arc<AtomicBool>,
    name: String,
    server_addr: String,
    transport: String,
    /// Whether this process has a next hop. The switch is meaningless without
    /// one, so the page and the API refuse to change it.
    upstream_configured: bool,
    /// The live value of `allow_upstream_modify`. A change is persisted first
    /// and then published here so the upstream session reconnects with the new
    /// capability bits.
    allow_upstream_modify: watch::Sender<bool>,
    /// When the management state was created, for the uptime of the console
    started: Instant,
    /// Tells the configuration watcher which file writes already took effect.
    reload: Arc<ReloadMarker>,
    /// The shared outcome of the most recent configuration watch/apply attempt.
    reload_status: Arc<crate::reload::ReloadStatus>,
    /// Reports a configuration applied live to the supervisor, together with
    /// the generation that applied it. The supervisor keeps its baseline in
    /// sync with the running process and ignores writes that raced with a
    /// reload.
    live: mpsc::Sender<(u64, Config)>,
    /// Asks the supervisor to rebuild the listener and upstream session with
    /// the given configuration, even when it equals the running one. The id
    /// makes a request from a replaced generation harmless.
    restart: mpsc::Sender<(u64, Config)>,
    /// The generation this management state belongs to.
    generation: u64,
}

impl ManagementState {
    pub(crate) fn new(
        config: Config,
        server: Arc<ServerState>,
        connected: Arc<AtomicBool>,
        reload: Arc<ReloadMarker>,
        reload_status: Arc<crate::reload::ReloadStatus>,
        generation: u64,
        live: mpsc::Sender<(u64, Config)>,
        restart: mpsc::Sender<(u64, Config)>,
    ) -> ManagementState {
        let config_path = config
            .source
            .path
            .clone()
            .unwrap_or_else(|| PathBuf::from("ht.toml"));

        let name = config.name.clone();
        let server_addr = config
            .upstream_addr()
            .unwrap_or_else(|| match config.listen_port() {
                Some(port) => crate::helper::to_bind_addr(config.bind_host(), port),
                None => "disabled".to_string(),
            });
        let transport = if config.upstream.is_some() {
            config.transport_name().to_string()
        } else {
            "ingress".to_string()
        };
        let upstream_configured = config.upstream.is_some();
        let (allow_upstream_modify, _) = watch::channel(config.allow_modify());

        ManagementState {
            config: RwLock::new(config),
            updates: Mutex::new(()),
            config_path,
            server,
            connected,
            name,
            server_addr,
            transport,
            upstream_configured,
            allow_upstream_modify,
            started: Instant::now(),
            reload,
            reload_status,
            live,
            restart,
            generation,
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

    /// The next hop may change the local `[[proxies]]`.
    pub(crate) fn allow_upstream_modify(&self) -> bool {
        *self.allow_upstream_modify.borrow()
    }

    /// A receiver that changes when `allow_upstream_modify` changes. The
    /// upstream session uses it to reconnect and re-announce its capabilities.
    pub(crate) fn subscribe_allow_upstream_modify(&self) -> watch::Receiver<bool> {
        self.allow_upstream_modify.subscribe()
    }

    /// Whether the web page may change the switch: it needs a next hop.
    pub(crate) fn allow_upstream_modify_editable(&self) -> bool {
        self.upstream_configured
    }

    /// The configuration file the API writes back to
    pub(crate) fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// The current configuration file content, used by the export endpoint.
    /// It returns the file as-is so that comments and formatting survive a
    /// backup; when the file is absent (a library caller built the config in
    /// memory), it falls back to the normalized TOML.
    pub(crate) async fn export_toml(&self) -> Result<String> {
        match Config::read(&self.config_path).await? {
            Some(content) => Ok(content),
            None => self.config_snapshot().await.to_toml(),
        }
    }

    /// Read and validate the configuration file without changing anything.
    pub(crate) async fn config_from_disk(&self) -> Result<Config> {
        let source = Config::read(&self.config_path).await?.ok_or_else(|| {
            anyhow!(
                "The configuration file {:?} does not exist",
                self.config_path
            )
        })?;
        let mut config = Config::parse(&source)
            .with_context(|| format!("The configuration {:?} is invalid", self.config_path))?;
        config.source = ConfigSource {
            path: Some(self.config_path.clone()),
        };
        Ok(config)
    }

    /// Ask the supervisor to rebuild the runtime after a short delay, so the
    /// management response that requested the restart can be flushed first.
    pub(crate) fn schedule_restart(&self, config: Config) {
        let restart = self.restart.clone();
        let generation = self.generation;
        tokio::spawn(async move {
            time::sleep(Duration::from_millis(500)).await;
            let _ = restart.send((generation, config)).await;
        });
    }

    /// Seconds since the process started, shown by the management page
    pub(crate) fn uptime(&self) -> u64 {
        self.started.elapsed().as_secs()
    }

    /// The outcome of the most recent configuration watch/apply attempt.
    pub(crate) fn reload_status(&self) -> crate::reload::ReloadSnapshot {
        self.reload_status.snapshot()
    }

    /// A snapshot of the running configuration.
    pub(crate) async fn config_snapshot(&self) -> Config {
        self.config.read().await.clone()
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

    /// The active visitor connections and the lifetime total, shown by the
    /// dashboard.
    pub(crate) fn visitor_stats(&self) -> (usize, u64) {
        self.server.visitor_stats()
    }

    /// Dial one saved local service from this node's perspective and report
    /// whether the target is reachable. A connection failure is a probe result,
    /// not an API error; only an unknown name is.
    pub(crate) async fn probe_proxy(&self, name: &str) -> Result<ProbeOutcome> {
        let name = name.trim();
        let proxy = self
            .proxies()
            .await
            .into_iter()
            .find(|proxy| proxy.name == name)
            .ok_or_else(|| anyhow!("No such a local service `{}`", name))?;

        let started = Instant::now();
        let (ok, error) = match time::timeout(
            PROBE_TIMEOUT,
            crate::helper::connect_local(&proxy.local_addr),
        )
        .await
        {
            Ok(Ok(_)) => (true, None),
            Ok(Err(error)) => (false, Some(format!("{:#}", error))),
            Err(_) => (
                false,
                Some(format!(
                    "Timed out after {} seconds",
                    PROBE_TIMEOUT.as_secs()
                )),
            ),
        };

        Ok(ProbeOutcome {
            name: proxy.name,
            local_addr: proxy.local_addr,
            ok,
            latency_ms: started.elapsed().as_millis() as u64,
            error,
        })
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
        if target.capabilities & protocol_v4::CAP_PROXY_AUTH == 0 {
            return Err(SessionAdminError::Unsupported(
                "The downstream node does not support `auth_header` management; upgrade it so \
                 both ends use the same proxy wire shape"
                    .to_string(),
            ));
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
                    auth_header: proxy.auth_header,
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
        auth_header: Option<MaskedString>,
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
                    auth_header,
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

    /// Persist a configuration change and apply it to the running generation.
    ///
    /// The file is written first, so a failed write leaves the runtime
    /// untouched. This path is only used for changes the running generation can
    /// take over without restarting its listener and upstream session.
    async fn persist_and_apply_live(&self, mut config: Config) -> Result<Config> {
        config.source = ConfigSource {
            path: Some(self.config_path.clone()),
        };
        // Validate after the change, so that a name or domain clash is rejected
        // before anything is written.
        Config::validate(&mut config)?;

        // Mark the configuration before the write. The watcher may notice the
        // new file before the runtime update below, and this mark tells it that
        // the process itself already applied the change.
        self.reload.mark_self_write(config.clone()).await;
        if let Err(err) = config.save(&self.config_path).await {
            self.reload.clear().await;
            return Err(err);
        }

        *self.config.write().await = config.clone();
        self.server.set_proxies(config.proxies.clone()).await;
        if self.allow_upstream_modify() != config.allow_modify() {
            self.allow_upstream_modify
                .send_replace(config.allow_modify());
        }
        // Keep the supervisor's baseline equal to what is running, so a later
        // external edit is compared against the applied configuration. This is
        // best-effort: a full channel only costs an unnecessary later restart.
        let _ = self.live.try_send((self.generation, config.clone()));
        Ok(config)
    }

    /// Persist a change of `[[proxies]]` or `allow_upstream_modify` live.
    async fn update_config<F>(&self, change: F) -> Result<Config>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        let _guard = self.updates.lock().await;
        let mut candidate = self.config.read().await.clone();
        candidate.source = ConfigSource::default();
        change(&mut candidate)?;
        self.persist_and_apply_live(candidate).await
    }

    /// Validate a settings change and report whether it needs a rebuild,
    /// without writing anything.
    pub(crate) async fn preview_settings<F>(&self, change: F) -> Result<SettingsOutcome>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        let _guard = self.updates.lock().await;
        let current = self.config.read().await.clone();
        let mut candidate = current.clone();
        candidate.source = ConfigSource::default();
        change(&mut candidate)?;
        Config::validate(&mut candidate)?;

        let restart_required = !crate::reload::same_effective_config(&current, &candidate)
            && !crate::reload::only_live_fields_changed(&current, &candidate);
        Ok(SettingsOutcome {
            config: candidate,
            restart_required,
        })
    }

    /// Change this node's full configuration.
    ///
    /// A change of `[[proxies]]` or `allow_upstream_modify` is applied live. A
    /// change of any other field is written to `ht.toml` and applied by the
    /// configuration watcher, which rebuilds the listener and upstream session.
    /// The caller learns whether the running generation was already updated.
    pub(crate) async fn update_settings<F>(
        &self,
        change: F,
        trigger_restart: bool,
    ) -> Result<SettingsOutcome>
    where
        F: FnOnce(&mut Config) -> Result<()>,
    {
        let _guard = self.updates.lock().await;
        let current = self.config.read().await.clone();
        let mut candidate = current.clone();
        candidate.source = ConfigSource::default();
        change(&mut candidate)?;
        Config::validate(&mut candidate)?;

        if crate::reload::same_effective_config(&current, &candidate) {
            return Ok(SettingsOutcome {
                config: current,
                restart_required: false,
            });
        }

        if crate::reload::only_live_fields_changed(&current, &candidate) {
            let applied = self.persist_and_apply_live(candidate).await?;
            return Ok(SettingsOutcome {
                config: applied,
                restart_required: false,
            });
        }

        // The watcher sees the new file and starts a new generation through the
        // normal reload path. Keep the management copy on the saved content so
        // a proxy edit made before that reload cannot write the old settings
        // back to the file. The listener keeps running the old generation until
        // the watcher restarts it.
        candidate.source = ConfigSource {
            path: Some(self.config_path.clone()),
        };
        // When the caller is about to request the rebuild itself, mark the
        // write so the file watcher does not start a second one.
        if trigger_restart {
            self.reload.mark_self_write(candidate.clone()).await;
        }
        if let Err(err) = candidate.save(&self.config_path).await {
            if trigger_restart {
                self.reload.clear().await;
            }
            return Err(err);
        }
        *self.config.write().await = candidate.clone();
        Ok(SettingsOutcome {
            config: candidate,
            restart_required: true,
        })
    }

    /// Apply a configuration file change that only touches the live fields.
    ///
    /// The supervisor uses this for an external edit of `[[proxies]]` or
    /// `allow_upstream_modify`, so adding a local service does not restart the
    /// listener and drop the visitors that are currently connected.
    #[cfg(feature = "client")]
    pub(crate) async fn apply_reloaded_config(&self, config: Config) {
        *self.config.write().await = config.clone();
        self.server.set_proxies(config.proxies.clone()).await;
        if self.allow_upstream_modify() != config.allow_modify() {
            self.allow_upstream_modify
                .send_replace(config.allow_modify());
        }
        info!(
            proxies = config.proxies.len(),
            allow_upstream_modify = config.allow_modify(),
            "The configuration file changed; applied the local services and switch"
        );
    }

    /// Create or replace the local service of the given name, persist the
    /// configuration, and apply the change.
    pub(crate) async fn put_proxy(
        &self,
        name: String,
        domain: String,
        local_addr: String,
        auth_header: Option<MaskedString>,
    ) -> Result<()> {
        if local_addr.trim().is_empty() {
            bail!("The `local_addr` must not be empty");
        }
        let domain = crate::helper::normalize_domain(&domain)
            .ok_or_else(|| anyhow!("The domain `{}` is invalid", domain))?;

        let effective = self
            .update_config(|config| {
                config.set_proxy(name, domain, local_addr, auth_header);
                Ok(())
            })
            .await?;
        info!(
            proxies = effective.proxies.len(),
            "The local services were updated"
        );
        Ok(())
    }

    /// Delete the local service of the given name, persist the configuration,
    /// and apply the change.
    pub(crate) async fn delete_proxy(&self, name: &str) -> Result<()> {
        let effective = self
            .update_config(|config| {
                if !config.remove_proxy(name.trim()) {
                    bail!("No such a local service `{}`", name);
                }
                Ok(())
            })
            .await?;
        info!(
            proxies = effective.proxies.len(),
            "The local services were updated"
        );
        Ok(())
    }

    /// Change `allow_upstream_modify`, persist it, and publish the new value so
    /// the upstream session reconnects with the new capability bits.
    pub(crate) async fn set_allow_upstream_modify(&self, allowed: bool) -> Result<()> {
        if !self.upstream_configured {
            bail!("No upstream is configured; this node is a standalone ingress");
        }

        let effective = self
            .update_config(|config| {
                let upstream = config
                    .upstream
                    .as_mut()
                    .ok_or_else(|| anyhow!("No upstream is configured"))?;
                upstream.allow_modify = allowed;
                Ok(())
            })
            .await?;
        info!(
            allow_upstream_modify = effective.allow_modify(),
            "The upstream proxy modification switch was updated"
        );
        Ok(())
    }
}

fn session_snapshot(
    target: &SessionTarget,
    proxies: Vec<ServerProxyEntry>,
) -> SessionProxySnapshot {
    SessionProxySnapshot {
        proxy_source: if target.capabilities & protocol_v4::CAP_PROXY_ENV != 0 {
            "env"
        } else {
            "file"
        },
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
            config
        };
        let server = Arc::new(ServerState::new(
            MaskedString::from("token"),
            config.name.clone(),
            config.bind_host().parse().ok(),
            config.entry_domain().map(str::to_string),
            config.server_limits(),
            routes,
            config.proxies.clone(),
        ));
        let (live, _live_rx) = mpsc::channel(8);
        let (restart, _restart_rx) = mpsc::channel(1);
        ManagementState::new(
            config,
            server,
            Arc::new(AtomicBool::new(true)),
            Arc::new(ReloadMarker::default()),
            Arc::new(crate::reload::ReloadStatus::new()),
            0,
            live,
            restart,
        )
    }

    #[tokio::test]
    async fn test_api_edits_persist_to_the_file() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-baseline.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            r#"
name = "file"

[server]
domain = "control.example.com"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        let state = state(persisted, path.clone());

        state
            .put_proxy(
                "git".to_string(),
                "git.example.com".to_string(),
                "127.0.0.1:81".to_string(),
                None,
            )
            .await?;

        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);

        assert!(saved.contains("file-token"), "{}", saved);
        assert!(saved.contains("git.example.com"), "{}", saved);
        assert!(saved.contains("[[proxies]]"), "{}", saved);
        assert_eq!(state.proxies().await.len(), 2);
        Ok(())
    }

    #[tokio::test]
    async fn test_api_sets_and_clears_the_auth_header() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-auth-header.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            r#"
name = "file"

[server]
domain = "control.example.com"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        std::fs::write(&path, persisted.to_toml()?)?;
        let state = state(persisted, path.clone());

        state
            .put_proxy(
                "nas".to_string(),
                "nas.example.com".to_string(),
                "127.0.0.1:80".to_string(),
                Some(MaskedString::from("visitor-secret")),
            )
            .await?;

        let proxies = state.proxies().await;
        assert_eq!(proxies[0].auth_header.as_deref(), Some("visitor-secret"));
        let saved = std::fs::read_to_string(&path)?;
        assert!(
            saved.contains("auth_header = \"visitor-secret\""),
            "{}",
            saved
        );

        // A PUT without auth_header replaces the entry and disables the gate
        state
            .put_proxy(
                "nas".to_string(),
                "nas.example.com".to_string(),
                "127.0.0.1:80".to_string(),
                None,
            )
            .await?;
        assert!(state.proxies().await[0].auth_header.is_none());
        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        assert!(!saved.contains("auth_header"), "{}", saved);
        Ok(())
    }

    #[tokio::test]
    async fn test_api_rejects_an_auth_header_with_line_breaks() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-auth-newline.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            r#"
name = "file"

[server]
domain = "control.example.com"
token = "file-token"
"#,
        )?;
        std::fs::write(&path, persisted.to_toml()?)?;
        let state = state(persisted, path.clone());

        let err = state
            .put_proxy(
                "nas".to_string(),
                "nas.example.com".to_string(),
                "127.0.0.1:80".to_string(),
                Some(MaskedString::from("bad\r\nheader")),
            )
            .await
            .expect_err("a line break in auth_header must be rejected");
        let _ = std::fs::remove_file(&path);
        assert!(
            format!("{:#}", err).contains("must not contain line breaks"),
            "{}",
            err
        );
        assert!(state.proxies().await.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn test_allow_upstream_modify_requires_an_upstream() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-no-upstream.toml",
            std::process::id()
        ));
        let persisted =
            Config::parse("[server]\ndomain = \"control.example.com\"\ntoken = \"secret\"\n")?;
        let _ = std::fs::remove_file(&path);
        let state = state(persisted, path.clone());

        assert!(!state.allow_upstream_modify());
        assert!(!state.allow_upstream_modify_editable());
        let err = state
            .set_allow_upstream_modify(true)
            .await
            .expect_err("an ingress has no upstream to authorize");
        assert!(format!("{:#}", err).contains("No upstream"), "{}", err);
        assert!(!path.exists(), "a rejected change must not write a config");
        Ok(())
    }

    #[tokio::test]
    async fn test_allow_upstream_modify_is_persisted_and_published() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-upstream-switch.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            "[server]\ndomain = \"control.example.com\"\ntoken = \"secret\"\n\n\
             [upstream]\nurl = \"ws://upstream.test:7777\"\ntoken = \"secret\"\n",
        )?;
        let _ = std::fs::remove_file(&path);
        let state = state(persisted, path.clone());
        let mut published = state.subscribe_allow_upstream_modify();

        assert!(state.allow_upstream_modify_editable());
        assert!(!state.allow_upstream_modify());
        state.set_allow_upstream_modify(true).await?;
        assert!(state.allow_upstream_modify());
        published.changed().await?;
        assert!(*published.borrow_and_update());

        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        assert!(saved.contains("allow_modify = true"), "{}", saved);
        Ok(())
    }

    #[tokio::test]
    async fn test_settings_are_persisted_and_live_when_only_the_switch_changes() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-management-{}-settings.toml",
            std::process::id()
        ));
        let persisted = Config::parse(
            "[server]\ndomain = \"control.example.com\"\ntoken = \"secret\"\n\n\
             [upstream]\nurl = \"ws://upstream.test:7777\"\ntoken = \"secret\"\n",
        )?;
        std::fs::write(&path, persisted.to_toml()?)?;
        let state = state(persisted, path.clone());

        let outcome = state
            .update_settings(
                |config| {
                    config
                        .upstream
                        .as_mut()
                        .expect("the test config has an upstream")
                        .allow_modify = true;
                    Ok(())
                },
                false,
            )
            .await?;
        assert!(!outcome.restart_required);
        assert!(state.allow_upstream_modify());
        let saved = std::fs::read_to_string(&path)?;
        assert!(saved.contains("allow_modify = true"), "{}", saved);

        let outcome = state
            .update_settings(
                |config| {
                    config.name = "renamed".to_string();
                    Ok(())
                },
                false,
            )
            .await?;
        assert!(outcome.restart_required);
        // The running generation keeps the old name until the watcher reloads.
        assert_eq!(state.name(), "ht");
        let saved = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        assert!(saved.contains("name = \"renamed\""), "{}", saved);
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
name = "file"

[server]
domain = "control.example.com"
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
                None,
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
