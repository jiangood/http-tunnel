use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use tokio::fs;

/// Application-layer heartbeat timeout in secs
const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 40;

/// The interval between retries to connect to the upstream
const DEFAULT_RETRY_INTERVAL_SECS: u64 = 1;

/// The default WebSocket ping interval in secs. Kept well below the usual 60 s
/// idle timeout of a reverse proxy or CDN.
const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// The default listener port of the universal process.
pub const DEFAULT_SERVER_PORT: u16 = 9000;

/// The default number of unauthenticated WebSocket upgrades a server runs at once.
pub const DEFAULT_MAX_PENDING_HANDSHAKES: usize = 256;
/// The default number of v4 sessions a server accepts at once.
pub const DEFAULT_MAX_CONTROL_CONNECTIONS: usize = 256;
/// The default cap on the routes the process advertises to its next hop.
pub const DEFAULT_MAX_MIRRORED_ROUTES: usize = 256;

/// String with Debug implementation that emits "MASKED"
/// Used to mask sensitive strings when logging
#[derive(Serialize, Deserialize, Default, PartialEq, Eq, Clone)]
pub struct MaskedString(String);

impl Debug for MaskedString {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::result::Result<(), std::fmt::Error> {
        f.write_str("MASKED")
    }
}

impl Deref for MaskedString {
    type Target = str;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<&str> for MaskedString {
    fn from(s: &str) -> MaskedString {
        MaskedString(String::from(s))
    }
}

impl From<String> for MaskedString {
    fn from(s: String) -> MaskedString {
        MaskedString(s)
    }
}

/// A local service served by the process itself.
///
/// The same entry serves as a server-side proxy of an ingress and as a local
/// service of a leaf or relay. A process with `[upstream]` advertises these
/// domains to its next hop (by mirroring, unless a single `claim` is set).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerProxyEntry {
    /// The identity of the local service, unique within the process
    pub name: String,
    /// The domain (the `Host` header) that is routed to this local service
    pub domain: String,
    /// The address of the local service, a TCP address or a `unix://` path
    pub local_addr: String,
    /// When set, a visitor must present `X-HTTP-TUNNEL-TOKEN: <value>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<MaskedString>,
}

/// The next hop towards the public ingress.
///
/// Without this table the process is a standalone ingress: it only runs its
/// embedded server. With it, the process also dials the next hop with one v4
/// session and advertises either one `claim` or a mirror of its route set.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The WebSocket endpoint of the next hop, e.g. `ws://a.lan:9000` or
    /// `wss://relay.example.com`.
    pub server: String,
    /// The shared token of the next hop.
    pub token: MaskedString,
    /// Claim one exact or wildcard domain on the next hop. This is the coarse
    /// relay mode: the upper hops see only this domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// Mirror the local `[[proxies]]` and the routes of downstream sessions to
    /// the next hop. It is the default when neither `mirror` nor `claim` is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<bool>,
}

impl UpstreamConfig {
    /// The domain claimed on the next hop, when this process is a coarse relay.
    pub fn claim(&self) -> Option<&str> {
        self.claim.as_deref()
    }

    /// Whether the route set of the process is mirrored to the next hop.
    pub fn mirrors(&self) -> bool {
        self.claim.is_none() && self.mirror != Some(false)
    }

    /// The `host:port` authority of the next hop, with an IPv6 host bracketed.
    pub fn server_addr(&self) -> Option<String> {
        let (_, authority) = split_server_url(&self.server)?;
        Some(authority.to_string())
    }

    /// Whether the upstream WebSocket connection uses TLS.
    pub fn tls(&self) -> bool {
        self.server.starts_with("wss://")
    }

    /// The normalized host and port of the next hop.
    pub(crate) fn host_port(&self) -> Result<(String, u16)> {
        parse_server_url(&self.server).map(|(_, host, port)| (host, port))
    }
}

/// The metadata of how a configuration was loaded. It is never serialized.
///
/// The administration API uses it to persist a change to the same file and to
/// keep environment values out of that file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ConfigSource {
    /// The file the configuration came from, if any
    pub(crate) path: Option<PathBuf>,
    /// The file baseline without the environment overlay
    pub(crate) persisted: Option<Box<Config>>,
    /// The environment settings that win over the file
    pub(crate) env: crate::client_env::EnvConfig,
    /// Whether the process started without a configuration file
    pub(crate) env_only: bool,
}

/// The configuration of the universal `ht` process.
///
/// One file describes both roles: the embedded listener/server (always running)
/// and one optional upstream session. Local services are declared once under
/// `[[proxies]]`, whether they are served directly by an ingress or advertised
/// to an upper hop by a leaf or relay.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The identity of the process, used for logging and the v4 node id
    #[serde(default = "default_name")]
    pub name: String,
    /// The listener port. `0` picks an ephemeral port and is only allowed when
    /// `[upstream]` is present.
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// The IP address the HTTP entrypoint binds. Defaults to `0.0.0.0`.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    /// The exact Host reserved for WebSocket control/data connections. It is
    /// required without `[upstream]`, and defaults to `127.0.0.1` with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_host: Option<String>,
    /// The shared token of downstream peers. When omitted with `[upstream]`, the
    /// upstream token is used for the listener too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<MaskedString>,
    /// The port of the web management page and API. `0` disables it.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub web_port: u16,
    /// The address the web management page and API bind. Defaults to `0.0.0.0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_host: Option<String>,
    /// Local services served by this process. `[[tunnels]]` is accepted as an
    /// alias for configurations written for older clients.
    #[serde(default, alias = "tunnels", skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<ServerProxyEntry>,
    /// The next hop. Remove this table to run as a standalone ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamConfig>,
    /// Application-layer heartbeat timeout of the upstream session, in secs.
    /// `0` disables the test.
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    /// The interval between retries to reach the upstream, in secs.
    #[serde(default = "default_retry_interval")]
    pub retry_interval: u64,
    /// The maximum number of routes mirrored to the next hop. `0` disables the
    /// cap.
    #[serde(default = "default_max_mirrored_routes")]
    pub max_mirrored_routes: usize,
    /// The number of authenticated WebSocket upgrades that may wait for their
    /// `ClientHello` at the same time.
    #[serde(default = "default_max_pending_handshakes")]
    pub max_pending_handshakes: usize,
    /// The number of v4 sessions accepted at once.
    #[serde(default = "default_max_control_connections")]
    pub max_control_connections: usize,
    /// WebSocket ping interval in secs. `0` disables keepalive pings.
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
    /// Deprecated: WebSocket sockets always use TCP_NODELAY. Accepted so that
    /// configurations written for the TCP transport stay loadable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodelay: Option<bool>,
    /// Load metadata, never written to TOML.
    #[serde(skip)]
    pub(crate) source: ConfigSource,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            server_port: default_server_port(),
            bind_host: default_bind_host(),
            server_host: None,
            token: None,
            web_port: 0,
            web_host: None,
            proxies: Vec::new(),
            upstream: None,
            heartbeat_timeout: default_heartbeat_timeout(),
            retry_interval: default_retry_interval(),
            max_mirrored_routes: default_max_mirrored_routes(),
            max_pending_handshakes: default_max_pending_handshakes(),
            max_control_connections: default_max_control_connections(),
            keepalive_interval: default_keepalive_interval(),
            nodelay: None,
            source: ConfigSource::default(),
        }
    }
}

impl Config {
    /// The address the web management page and API bind.
    pub fn web_host(&self) -> &str {
        self.web_host
            .as_deref()
            .unwrap_or(crate::helper::DEFAULT_BIND_HOST)
    }

    /// The token the embedded listener authenticates downstream peers with.
    ///
    /// When the top-level `token` is omitted and `[upstream]` is present, the
    /// upstream token is reused for the listener.
    pub fn listener_token(&self) -> Option<&MaskedString> {
        self.token
            .as_ref()
            .or_else(|| self.upstream.as_ref().map(|upstream| &upstream.token))
    }

    /// The resolved `server_host`, which validation always fills in.
    pub fn server_host(&self) -> &str {
        self.server_host
            .as_deref()
            .unwrap_or(crate::helper::SERVER_HOST_FALLBACK)
    }

    /// The runtime limits of the embedded server.
    pub fn server_limits(&self) -> ServerLimits {
        ServerLimits {
            max_pending_handshakes: self.max_pending_handshakes,
            max_control_connections: self.max_control_connections,
            keepalive_interval: self.keepalive_interval,
        }
    }

    /// The WebSocket scheme of the upstream, `wss` when there is none.
    pub fn transport_name(&self) -> &str {
        self.upstream
            .as_ref()
            .and_then(|upstream| split_server_url(&upstream.server).map(|(scheme, _)| scheme))
            .unwrap_or("wss")
    }

    /// Whether the upstream connection uses TLS.
    pub fn tls(&self) -> bool {
        self.upstream
            .as_ref()
            .map(UpstreamConfig::tls)
            .unwrap_or(false)
    }

    /// The `host:port` authority of the upstream, if one is configured.
    pub fn upstream_addr(&self) -> Option<String> {
        self.upstream.as_ref().and_then(UpstreamConfig::server_addr)
    }

    /// Replace the local service of the given name, or add it.
    pub fn set_proxy(&mut self, name: String, domain: String, local_addr: String) {
        match self.proxies.iter_mut().find(|entry| entry.name == name) {
            Some(entry) => {
                entry.domain = domain;
                entry.local_addr = local_addr;
            }
            None => self.proxies.push(ServerProxyEntry {
                name,
                domain,
                local_addr,
                auth_header: None,
            }),
        }
    }

    /// Remove the local service of the given name. Returns whether one was removed.
    pub fn remove_proxy(&mut self, name: &str) -> bool {
        let before = self.proxies.len();
        self.proxies.retain(|entry| entry.name != name);
        self.proxies.len() != before
    }

    /// Parse a configuration and normalize it.
    pub fn parse(s: &str) -> Result<Config> {
        let mut config: Config = toml::from_str(s)
            .map_err(|e| {
                if has_setting_at_top_level(s, "remote") {
                    anyhow!(
                        "{}\nNote: `remote` is no longer supported. Set `[upstream].server = \
                         \"wss://host:port\"` instead.",
                        e
                    )
                } else if has_setting_at_top_level(s, "server") {
                    anyhow!(
                        "{}\nNote: the next hop belongs in the `[upstream]` table. Move \
                         `server = ...` there.",
                        e
                    )
                } else if has_setting(s, "http_port") || has_setting(s, "control_host") {
                    anyhow!(
                        "{}\nNote: `http_port` and `control_host` were renamed to \
                         `server_port` and `server_host`.",
                        e
                    )
                } else {
                    anyhow!("{}", e)
                }
            })
            .with_context(|| "Failed to parse the config")?;

        Config::validate(&mut config)?;

        Ok(config)
    }

    /// Read the configuration file. If it is missing, write the commented
    /// example and ask the user to edit it before starting.
    pub async fn from_file(path: &Path) -> Result<Config> {
        match Config::read(path).await? {
            Some(s) => {
                let mut config = Config::parse(&s)
                    .with_context(|| format!("The configuration {:?} is invalid", path))?;
                config.source.path = Some(path.to_path_buf());
                let mut persisted = config.clone();
                persisted.source = ConfigSource::default();
                config.source.persisted = Some(Box::new(persisted));
                Ok(config)
            }
            None => Err(Config::bootstrap_example(path).await),
        }
    }

    /// Read the configuration file, `None` when it does not exist.
    pub async fn read(path: &Path) -> Result<Option<String>> {
        match fs::read_to_string(path).await {
            Ok(s) => Ok(Some(s)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }

    /// Write the commented example and return the error that asks the user to
    /// edit it before the first start.
    pub(crate) async fn bootstrap_example(path: &Path) -> anyhow::Error {
        match fs::write(path, DEFAULT_CONFIG).await {
            Ok(()) => anyhow!(
                "Generated {:?}. Set `server_host` and a strong `token`, add the `[upstream]` \
                 settings for a leaf or relay, then restart ht.",
                path
            ),
            Err(e) => anyhow::Error::new(e)
                .context(format!("Failed to generate default config {:?}", path)),
        }
    }

    /// Validate a configuration and normalize it in place.
    ///
    /// The rules keep the process from becoming an accidental open relay or a
    /// self-loop: the tokens must be set, the local routes must not overlap the
    /// reserved `server_host`, and the upstream must not point back at this
    /// process's own listener.
    pub fn validate(config: &mut Config) -> Result<()> {
        let name = config.name.trim();
        if name.is_empty() {
            bail!("`name` must not be empty");
        }
        if name.contains('\r') || name.contains('\n') {
            bail!("`name` must not contain line breaks");
        }
        config.name = name.to_string();

        let bind_host = normalize_bind_host(&mut config.bind_host, "bind_host")?;
        if let Some(web_host) = config.web_host.as_mut() {
            normalize_bind_host(web_host, "web_host")?;
        }

        let has_upstream = config.upstream.is_some();
        if config.server_port == 0 && !has_upstream {
            bail!(
                "`server_port` may only be 0 when `[upstream]` is present: a standalone \
                 ingress has no upstream endpoint to discover its ephemeral port"
            );
        }
        if has_upstream && !(bind_host.is_loopback() || bind_host.is_unspecified()) {
            bail!(
                "`bind_host` must be `0.0.0.0` or a loopback address when `[upstream]` is \
                 present: the upstream tunnel forwards visitors to `127.0.0.1:<server_port>`"
            );
        }

        let mut server_host = match config.server_host.take() {
            Some(host) => host,
            None if has_upstream => crate::helper::SERVER_HOST_FALLBACK.to_string(),
            None => bail!(
                "`server_host` is required without `[upstream]`: it is the reserved Host of \
                 the WebSocket control endpoint"
            ),
        };
        let normalized = crate::helper::normalize_domain(&server_host)
            .ok_or_else(|| anyhow!("Invalid server_host"))?;
        if normalized.starts_with("*.") {
            bail!("server_host must be an exact hostname, not a wildcard");
        }
        server_host = normalized;
        config.server_host = Some(server_host.clone());

        let listener_token = config
            .listener_token()
            .ok_or_else(|| anyhow!("Set a strong shared `token` for the listener"))?;
        if is_placeholder_token(listener_token) {
            bail!("Set a strong, non-placeholder `token`");
        }

        let limits = config.server_limits();
        limits.validate()?;

        let mut names: BTreeMap<String, ()> = BTreeMap::new();
        let mut domains: BTreeMap<String, ()> = BTreeMap::new();
        let mut entries: Vec<ServerProxyEntry> = Vec::new();
        for entry in std::mem::take(&mut config.proxies) {
            if entry.local_addr.trim().is_empty() {
                bail!(
                    "The `local_addr` of the local service `{}` is empty",
                    entry.domain
                );
            }
            if entry
                .auth_header
                .as_ref()
                .is_some_and(|value| value.contains('\r') || value.contains('\n'))
            {
                bail!(
                    "The `auth_header` of the local service `{}` must not contain line breaks",
                    entry.name
                );
            }

            let domain = crate::helper::normalize_domain(&entry.domain)
                .ok_or_else(|| anyhow!("The domain `{}` is invalid", entry.domain))?;
            let name = validate_name(&entry.name)?;

            if domain_overlaps_server_host(&domain, &server_host) {
                bail!(
                    "The local service domain `{}` overlaps the reserved server_host `{}`",
                    domain,
                    server_host
                );
            }
            if domains.insert(domain.clone(), ()).is_some() {
                bail!("The domain `{}` is defined more than once", domain);
            }
            if names.insert(name.clone(), ()).is_some() {
                bail!(
                    "The local service name `{}` is defined more than once",
                    name
                );
            }

            entries.push(ServerProxyEntry {
                name,
                domain,
                local_addr: entry.local_addr,
                auth_header: entry.auth_header,
            });
        }
        config.proxies = entries;

        let Some(upstream) = config.upstream.as_mut() else {
            return Ok(());
        };

        let (server, host, port) = canonical_server_url(&upstream.server)?;
        upstream.server = server;
        if upstream.token.is_empty() || is_placeholder_token(&upstream.token) {
            bail!("Set a strong, non-placeholder `[upstream].token` for the next hop");
        }

        match (upstream.claim.as_ref(), upstream.mirror) {
            (Some(_), Some(true)) => bail!(
                "Set either `[upstream].claim` (one exact or wildcard domain) or \
                 `mirror = true` (one control channel per route), not both"
            ),
            (None, Some(false)) => bail!(
                "Set `[upstream].claim` for one exact or wildcard domain, or `mirror = true` \
                 to mirror the routes of this process"
            ),
            (None, None) => upstream.mirror = Some(true),
            (Some(_), None) => upstream.mirror = Some(false),
            (Some(_), Some(false)) | (None, Some(true)) => {}
        }

        if let Some(claim) = upstream.claim.take() {
            let claim = crate::helper::normalize_domain(&claim)
                .ok_or_else(|| anyhow!("The `[upstream].claim` `{}` is invalid", claim))?;
            if domain_overlaps_server_host(&claim, &server_host) {
                bail!(
                    "The `[upstream].claim` `{}` overlaps the reserved server_host `{}`",
                    claim,
                    server_host
                );
            }
            // The host of the upstream URL is the `server_host` of the next hop,
            // because the WebSocket client sends it as the `Host` header. A
            // claim covering it can never be registered there.
            let upstream_host = if host.contains(':') && !host.starts_with('[') {
                format!("[{}]", host)
            } else {
                host.clone()
            };
            let upstream_host = upstream_host.to_ascii_lowercase();
            if domain_overlaps_server_host(&claim, &upstream_host) {
                bail!(
                    "The `[upstream].claim` `{}` overlaps the next hop's server_host `{}`",
                    claim,
                    upstream_host
                );
            }
            upstream.claim = Some(claim);
        }

        // A same-port endpoint with the reserved host or a loopback/unspecified
        // address dials this process's own listener. A port of 0 is resolved at
        // startup, after the listener has picked the actual port.
        if config.server_port != 0
            && port == config.server_port
            && (host.eq_ignore_ascii_case(&server_host) || is_local_host(&host))
        {
            bail!(
                "`[upstream].server` points at this process's own listener ({}:{}); configure \
                 the next hop instead of itself",
                host,
                port
            );
        }

        Ok(())
    }

    /// Serialize the configuration back to TOML.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).with_context(|| "Failed to serialize the config")
    }

    /// Write the configuration back to the file atomically, so that a change made
    /// through the administration API survives a restart.
    pub async fn save(&self, path: &Path) -> Result<()> {
        let s = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, s.as_bytes())
            .await
            .with_context(|| format!("Failed to write the temporary config {:?}", tmp))?;
        fs::rename(&tmp, path)
            .await
            .with_context(|| format!("Failed to replace the config {:?}", path))?;
        Ok(())
    }
}

fn default_name() -> String {
    "ht".to_string()
}
fn default_server_port() -> u16 {
    DEFAULT_SERVER_PORT
}
fn default_bind_host() -> String {
    crate::helper::DEFAULT_BIND_HOST.to_string()
}
fn default_heartbeat_timeout() -> u64 {
    DEFAULT_HEARTBEAT_TIMEOUT_SECS
}
fn default_retry_interval() -> u64 {
    DEFAULT_RETRY_INTERVAL_SECS
}
fn default_max_mirrored_routes() -> usize {
    DEFAULT_MAX_MIRRORED_ROUTES
}
fn default_max_pending_handshakes() -> usize {
    DEFAULT_MAX_PENDING_HANDSHAKES
}
fn default_max_control_connections() -> usize {
    DEFAULT_MAX_CONTROL_CONNECTIONS
}
fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_SECS
}
fn is_zero(value: &u16) -> bool {
    *value == 0
}

/// Whether a token is one of the documented placeholders rather than a secret.
fn is_placeholder_token(token: &str) -> bool {
    token.starts_with("replace-with-") || token.trim().is_empty()
}

/// Format a host and a port as a `host:port` authority, bracketing IPv6 hosts.
pub(crate) fn format_server_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Whether the TOML source declares a setting with the given key.
fn has_setting(s: &str, key: &str) -> bool {
    s.lines().any(|line| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Whether the TOML source declares a setting before the first table.
fn has_setting_at_top_level(s: &str, key: &str) -> bool {
    s.lines()
        .map(str::trim_start)
        .take_while(|line| !line.starts_with('['))
        .any(|line| {
            line.strip_prefix(key)
                .is_some_and(|rest| rest.trim_start().starts_with('='))
        })
}

/// Validate the `server` endpoint and store it in a canonical form.
///
/// The canonical form is a lowercase scheme, a host and an explicit, validated
/// port, so the rest of the process can split it without parsing again.
fn canonical_server_url(raw: &str) -> Result<(String, String, u16)> {
    let (scheme, host, port) = parse_server_url(raw)?;
    Ok((
        format!("{}://{}", scheme, format_server_addr(&host, port)),
        host,
        port,
    ))
}

/// Split a canonical `scheme://host:port` endpoint into its scheme and authority.
fn split_server_url(server: &str) -> Option<(&str, &str)> {
    server.split_once("://")
}

/// Parse a `ws://` or `wss://` endpoint into its scheme, host and port.
///
/// The port is optional and defaults to 443 for `wss` and 80 for `ws`. The
/// path, query and fragment are rejected because the control path is fixed by
/// the protocol.
fn parse_server_url(raw: &str) -> Result<(String, String, u16)> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("The `[upstream].server` setting must not be empty");
    }
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        anyhow!(
            "The `[upstream].server` setting must be a WebSocket URL such as \
             \"wss://control.example.com\""
        )
    })?;
    let scheme = scheme.trim().to_ascii_lowercase();
    if scheme != "ws" && scheme != "wss" {
        bail!(
            "The `[upstream].server` scheme must be `ws` or `wss`, not `{}`",
            scheme
        );
    }
    if rest.is_empty() {
        bail!("The `[upstream].server` host is missing");
    }
    // A trailing slash is the root path and is harmless; the control path is
    // appended by the protocol itself.
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains('/') || rest.contains('\\') || rest.contains('?') || rest.contains('#') {
        bail!(
            "The `[upstream].server` setting must contain only a host and an optional port, \
             without a path, query or fragment"
        );
    }

    let default_port = if scheme == "wss" { 443 } else { 80 };
    let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            anyhow!(
                "The `[upstream].server` host `{}` is not a valid IPv6 address",
                raw
            )
        })?;
        let port = if tail.is_empty() {
            default_port
        } else if let Some(port) = tail.strip_prefix(':') {
            port.parse::<u16>().map_err(|_| {
                anyhow!(
                    "The `[upstream].server` port `{}` is not a valid port",
                    port
                )
            })?
        } else {
            bail!("The `[upstream].server` host `{}` is invalid", raw);
        };
        (format!("[{}]", host), port)
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                let port = port.parse::<u16>().map_err(|_| {
                    anyhow!(
                        "The `[upstream].server` port `{}` is not a valid port",
                        port
                    )
                })?;
                (host.to_string(), port)
            }
            Some(_) => {
                bail!("The IPv6 host of `[upstream].server` must be bracketed, e.g. \"wss://[::1]:443\"")
            }
            None => (rest.to_string(), default_port),
        }
    };
    let host = normalize_server_host(&host)?;
    if port == 0 {
        bail!("The `[upstream].server` port must not be 0");
    }
    Ok((scheme, host, port))
}

/// Normalize and validate a server host.
///
/// A host is a DNS name, an IPv4 address or an IPv6 address. The port belongs to
/// `server`, so a `host:port` value is rejected with a hint. IPv6 literals may be
/// written with or without brackets.
fn normalize_server_host(raw: &str) -> Result<String> {
    let host = raw.trim();
    if host.is_empty() {
        bail!("The `[upstream].server` host must not be empty");
    }
    if host.contains("://") {
        bail!("The `[upstream].server` host must not include a scheme");
    }
    if host.chars().any(|c| c.is_whitespace()) {
        bail!("The `[upstream].server` host must not contain whitespace");
    }
    if host.contains('/')
        || host.contains('\\')
        || host.contains('?')
        || host.contains('#')
        || host.contains('@')
    {
        bail!("The server host `{}` is not a valid host", raw);
    }

    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The `[upstream].server` host `{}` is not a valid IPv6 address",
                raw
            )
        })?;
        return Ok(inner.to_string());
    }
    if host.contains('[') || host.contains(']') {
        bail!("The server host `{}` is not a valid host", raw);
    }
    if host.contains(':') {
        host.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The server host `{}` must not include a port; put the port in \
                 `[upstream].server` instead",
                raw
            )
        })?;
    }
    Ok(host.to_string())
}

/// Validate a local-service name and return it trimmed.
fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("A `[[proxies]].name` must not be empty");
    }
    let valid = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !valid {
        bail!(
            "The local service name `{}` is invalid: only letters, digits, dot, \
             underscore and hyphen are allowed",
            name
        );
    }
    Ok(name.to_string())
}

/// Whether a proxy domain (exact or wildcard) would overlap the reserved
/// `server_host`. A wildcard overlaps every host below its suffix.
pub(crate) fn domain_overlaps_server_host(domain: &str, server_host: &str) -> bool {
    domain == server_host
        || domain.strip_prefix("*.").is_some_and(|suffix| {
            server_host.len() > suffix.len() && server_host.ends_with(&format!(".{}", suffix))
        })
}

/// Whether a host is the local machine in the forms that cannot be a remote
/// hop when the port is the process's own.
fn is_local_host(host: &str) -> bool {
    let bare = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    bare.eq_ignore_ascii_case("localhost")
        || bare
            .parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback() || ip.is_unspecified())
            .unwrap_or(false)
}

/// Parse and canonicalize an IP address that a listener binds.
fn normalize_bind_host(host: &mut String, field: &str) -> Result<std::net::IpAddr> {
    let ip = host.trim().parse::<std::net::IpAddr>().map_err(|_| {
        anyhow!(
            "`{}` must be an IP address such as `0.0.0.0` or `127.0.0.1`, got `{}`",
            field,
            host
        )
    })?;
    *host = ip.to_string();
    Ok(ip)
}

/// The runtime limits of the embedded server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLimits {
    pub max_pending_handshakes: usize,
    pub max_control_connections: usize,
    pub keepalive_interval: u64,
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_pending_handshakes: DEFAULT_MAX_PENDING_HANDSHAKES,
            max_control_connections: DEFAULT_MAX_CONTROL_CONNECTIONS,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL_SECS,
        }
    }
}

impl ServerLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pending_handshakes == 0 || self.max_control_connections == 0 {
            bail!("The connection limits must be greater than 0");
        }
        Ok(())
    }
}

/// The example written when `ht.toml` is missing.
///
/// `examples/minimal/ht.toml` is the single source of truth: it is embedded at
/// compile time, so the shipped binary needs no example directory at runtime.
pub(crate) const DEFAULT_CONFIG: &str = include_str!("../examples/minimal/ht.toml");

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, path::PathBuf};

    use anyhow::Result;

    fn list_config_files<T: AsRef<Path>>(root: T) -> Result<Vec<PathBuf>> {
        let mut files = Vec::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            } else if path.is_dir() {
                files.append(&mut list_config_files(path)?);
            }
        }
        Ok(files)
    }

    fn get_all_example_config() -> Result<Vec<PathBuf>> {
        Ok(list_config_files("./examples")?
            .into_iter()
            .filter(|x| x.extension().map(|e| e == "toml").unwrap_or(false))
            .collect())
    }

    fn router(source: &str) -> Config {
        Config::parse(source).expect("the test source should be a valid config")
    }

    #[test]
    fn test_server_url_validation() -> Result<()> {
        let ws = router(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             server = \"ws://example.com\"\ntoken = \"secret\"\n",
        );
        assert_eq!(ws.upstream.as_ref().unwrap().server, "ws://example.com:80");
        assert_eq!(ws.transport_name(), "ws");
        assert!(!ws.tls());

        assert!(Config::parse(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             server = \"udp://example.com\"\ntoken = \"secret\"\n"
        )
        .is_err());
        assert!(Config::parse(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             server = \"example.com:443\"\ntoken = \"secret\"\n"
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn test_server_addr() -> Result<()> {
        let cfg = router(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             server = \"wss://example.com:2333\"\ntoken = \"secret\"\n",
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().server_addr().as_deref(),
            Some("example.com:2333")
        );

        let cfg = router(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             server = \"wss://[::1]\"\ntoken = \"secret\"\n",
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().server_addr().as_deref(),
            Some("[::1]:443")
        );
        Ok(())
    }

    #[test]
    fn test_rejects_an_invalid_server_address() {
        for server in [
            "example.com:2333",
            "tcp://example.com",
            "",
            "wss://",
            "wss://example.com:0",
            "wss://example.com/path",
            "wss://::1:443",
            "wss://user@example.com",
        ] {
            let source = format!(
                "server_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
                 server = {:?}\ntoken = \"secret\"\n",
                server
            );
            assert!(
                Config::parse(&source).is_err(),
                "{} is valid upstream",
                server
            );
        }
    }

    #[test]
    fn test_example_config() -> Result<()> {
        let paths = get_all_example_config()?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(&p)?;
            let mut config: Config = toml::from_str(&s)?;
            // The shipped template carries placeholder tokens, so the strict
            // validation must reject it until it is edited.
            assert!(Config::validate(&mut config).is_err(), "{:?} is valid", p);
            config.token = Some(MaskedString::from("downstream-test-token"));
            config.upstream.as_mut().unwrap().token = MaskedString::from("upstream-test-token");
            Config::validate(&mut config)?;
            assert_eq!(config.name, "edge");
            assert!(config.upstream.as_ref().unwrap().mirrors());
            assert!(config.upstream.as_ref().unwrap().claim().is_none());
        }
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(p)?;
            Config::parse(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(&p)?;
            assert!(Config::parse(&s).is_err(), "{:?} is valid", p);
        }
        Ok(())
    }

    #[test]
    fn test_resolved_proxies() -> Result<()> {
        let s = r#"
server_host = "control.test"
name = "edge"
token = "secret"
retry_interval = 7

[[proxies]]
name = "nas"
domain = "Foo1.Example.COM"
local_addr = "127.0.0.1:80"

[[proxies]]
name = "foo2"
domain = "foo2.example.com"
local_addr = "127.0.0.1:81"
"#;
        let cfg = router(s);
        assert_eq!(cfg.proxies.len(), 2);
        assert_eq!(cfg.proxies[0].name, "nas");
        assert_eq!(cfg.proxies[0].domain, "foo1.example.com");
        assert_eq!(cfg.proxies[0].local_addr, "127.0.0.1:80");
        assert_eq!(cfg.proxies[1].name, "foo2");
        assert_eq!(cfg.proxies[1].domain, "foo2.example.com");
        assert_eq!(cfg.proxies[1].local_addr, "127.0.0.1:81");
        assert_eq!(cfg.retry_interval, 7);
        Ok(())
    }

    #[test]
    fn test_tunnels_is_an_alias_for_proxies() -> Result<()> {
        let cfg = router(
            "server_host = \"control.test\"\ntoken = \"secret\"\n\n[[tunnels]]\n\
             name = \"nas\"\ndomain = \"nas.example.com\"\n\
             local_addr = \"127.0.0.1:80\"\n",
        );
        assert_eq!(cfg.proxies.len(), 1);
        assert_eq!(cfg.proxies[0].name, "nas");
        Ok(())
    }

    #[test]
    fn test_rejects_the_legacy_tunnels_table() {
        // The old `[tunnels]` table (domain -> local_addr) is no longer accepted
        let s = r#"
server_host = "control.test"
name = "home"
token = "secret"

[tunnels]
"nas.example.com" = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_set_and_remove_proxy() -> Result<()> {
        let mut cfg = router("server_host = \"control.test\"\ntoken = \"secret\"\n");

        cfg.set_proxy(
            "nas".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        Config::validate(&mut cfg)?;
        assert_eq!(cfg.proxies.len(), 1);

        // Replacing by name changes the domain without adding a proxy
        cfg.set_proxy(
            "nas".to_string(),
            "nas2.example.com".to_string(),
            "127.0.0.1:81".to_string(),
        );
        Config::validate(&mut cfg)?;
        let proxies = &cfg.proxies;
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].domain, "nas2.example.com");
        assert_eq!(proxies[0].local_addr, "127.0.0.1:81");

        // Removing by name
        assert!(cfg.remove_proxy("nas"));
        assert!(!cfg.remove_proxy("nas"));
        assert!(cfg.proxies.is_empty());
        Ok(())
    }

    #[test]
    fn test_set_proxy_adds_and_keeps_names_distinct() -> Result<()> {
        let mut cfg = router("server_host = \"control.test\"\ntoken = \"secret\"\n");

        cfg.set_proxy(
            "app-1".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        // A different name serving the same domain is not adopted: it clashes
        cfg.set_proxy(
            "app-2".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        assert!(Config::validate(&mut cfg).is_err());

        // Replacing by name is fine
        cfg.set_proxy(
            "app-1".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:81".to_string(),
        );
        Config::validate(&mut cfg)?;
        assert_eq!(cfg.proxies.len(), 1);
        assert_eq!(cfg.proxies[0].name, "app-1");
        assert_eq!(cfg.proxies[0].local_addr, "127.0.0.1:81");
        Ok(())
    }

    #[test]
    fn test_defaults() -> Result<()> {
        let cfg = Config::default();
        assert_eq!(cfg.name, "ht");
        assert_eq!(cfg.server_port, DEFAULT_SERVER_PORT);
        assert_eq!(cfg.bind_host, "0.0.0.0");
        assert_eq!(cfg.web_port, 0);
        assert_eq!(cfg.heartbeat_timeout, DEFAULT_HEARTBEAT_TIMEOUT_SECS);
        assert_eq!(cfg.retry_interval, DEFAULT_RETRY_INTERVAL_SECS);
        assert!(cfg.proxies.is_empty());
        Ok(())
    }

    #[test]
    fn test_token_falls_back_to_the_upstream_token() -> Result<()> {
        let cfg = router(
            "server_host = \"control.test\"\n\n[upstream]\nserver = \"ws://a.test:9000\"\n\
             token = \"upstream-secret\"\n",
        );
        assert!(cfg.token.is_none());
        assert_eq!(&**cfg.listener_token().unwrap(), "upstream-secret");
        let dumped = cfg.to_toml()?;
        assert!(
            !dumped.contains("token = \"upstream-secret\"\nserver_host"),
            "{}",
            dumped
        );
        Ok(())
    }

    #[test]
    fn test_rejects_an_empty_local_addr() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "foo"
domain = "foo.example.com"
local_addr = ""
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_invalid_domain() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "foo"
domain = "foo bar"
local_addr = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_domain() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "foo1"
domain = "foo.example.com"
local_addr = "127.0.0.1:80"

[[proxies]]
name = "foo2"
domain = "Foo.Example.com"
local_addr = "127.0.0.1:81"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_unknown_field() {
        let s = r#"
server_host = "control.test"
token = "secret"
tokenz = "oops"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_name() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"

[[proxies]]
name = "nas"
domain = "git.example.com"
local_addr = "127.0.0.1:81"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_missing_name() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
domain = "foo.example.com"
local_addr = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_missing_domain() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "nas"
local_addr = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_invalid_name() {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "nas/../etc"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_parse_rejects_legacy_settings_with_a_hint() {
        let legacy = r#"
server = "wss://control.example.com:443"
name = "home"
token = "123"
"#;
        let err = format!("{:#}", Config::parse(legacy).unwrap_err());
        assert!(err.contains("[upstream]"), "{}", err);

        let legacy = "http_port = 8080\ncontrol_host = \"control.test\"\ntoken = \"secret\"\n";
        let err = format!("{:#}", Config::parse(legacy).unwrap_err());
        assert!(err.contains("renamed to"), "{}", err);
    }

    #[test]
    fn test_config_roundtrip() -> Result<()> {
        let s = r#"
name = "edge"
server_port = 9000
bind_host = "127.0.0.1"
server_host = "control.test"
token = "secret"
web_port = 8611
retry_interval = 2

[[proxies]]
name = "nas"
domain = "foo1.example.com"
local_addr = "127.0.0.1:80"

[[proxies]]
name = "git"
domain = "foo2.example.com"
local_addr = "127.0.0.1:81"

[upstream]
server = "ws://a.test:9000"
token = "upstream-secret"
claim = "*.edge.test"
"#;
        let cfg = router(s);
        let dumped = cfg.to_toml()?;
        let reparsed = Config::parse(&dumped)?;
        assert_eq!(cfg, reparsed);
        assert!(dumped.contains("[[proxies]]"), "{}", dumped);
        assert!(
            dumped.contains("domain = \"foo1.example.com\""),
            "{}",
            dumped
        );
        assert!(
            dumped.contains("local_addr = \"127.0.0.1:80\""),
            "{}",
            dumped
        );
        Ok(())
    }

    #[test]
    fn test_config_roundtrip_without_optional_fields() -> Result<()> {
        let s = r#"
server_host = "control.test"
token = "secret"

[[proxies]]
name = "foo1"
domain = "foo1.example.com"
local_addr = "127.0.0.1:80"
"#;
        let cfg = router(s);
        let dumped = cfg.to_toml()?;
        assert!(!dumped.contains("nodelay"));
        assert!(!dumped.contains("upstream"));
        let reparsed = Config::parse(&dumped)?;
        assert_eq!(cfg, reparsed);
        Ok(())
    }

    #[test]
    fn test_default_config_is_not_runnable_but_parses_as_toml() {
        let cfg: Config = toml::from_str("").unwrap();
        assert_eq!(cfg, Config::default());
        assert!(Config::validate(&mut Config::default()).is_err());
    }

    #[test]
    fn test_domain_cannot_overlap_server_host() {
        assert!(domain_overlaps_server_host(
            "control.example.com",
            "control.example.com"
        ));
        assert!(domain_overlaps_server_host(
            "*.example.com",
            "control.example.com"
        ));
        assert!(!domain_overlaps_server_host("*.example.com", "example.com"));
        assert!(!domain_overlaps_server_host(
            "*.other.test",
            "control.example.com"
        ));
    }

    #[tokio::test]
    async fn test_server_host_defaults_to_loopback_with_upstream() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-server-host-{}.toml",
            std::process::id()
        ));

        // Without upstream, the reserved Host is required.
        std::fs::write(&path, "server_port = 8080\ntoken = \"a_secret_token\"\n")?;
        let err = Config::from_file(&path)
            .await
            .expect_err("a missing server_host must be rejected");
        assert!(
            format!("{:#}", err).contains("server_host"),
            "unexpected error: {:#}",
            err
        );

        // With upstream it defaults to loopback.
        std::fs::write(
            &path,
            "server_port = 8080\ntoken = \"a_secret_token\"\n\n[upstream]\n\
             server = \"ws://a.test:9000\"\ntoken = \"upstream-secret\"\n",
        )?;
        let cfg = Config::from_file(&path).await?;
        assert_eq!(cfg.server_host(), "127.0.0.1");

        // The generated template must not start with its placeholder token
        std::fs::write(&path, DEFAULT_CONFIG)?;
        let err = Config::from_file(&path)
            .await
            .expect_err("the placeholder token must be rejected");
        assert!(format!("{:#}", err).contains("token"), "{:#}", err);

        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[tokio::test]
    async fn test_bind_host_defaults_to_all_interfaces_and_is_validated() -> Result<()> {
        let path =
            std::env::temp_dir().join(format!("http-tunnel-bind-host-{}.toml", std::process::id()));
        let base = "server_port = 8080\nserver_host = \"control.example.com\"\n\
                    token = \"a_secret_token\"\n";

        // Existing configurations keep binding every interface
        std::fs::write(&path, base)?;
        let config = Config::from_file(&path).await?;
        assert_eq!(config.bind_host, "0.0.0.0");

        // A loopback (or IPv6) address is accepted and canonicalized
        std::fs::write(&path, format!("{}bind_host = \"::1\"\n", base))?;
        let config = Config::from_file(&path).await?;
        assert_eq!(config.bind_host, "::1");

        for invalid in ["localhost", "0.0.0.0:80", "  "] {
            std::fs::write(&path, format!("{}bind_host = {:?}\n", base, invalid))?;
            let err = Config::from_file(&path)
                .await
                .expect_err("a non-IP bind_host must be rejected");
            assert!(
                format!("{:#}", err).contains("bind_host"),
                "unexpected error: {:#}",
                err
            );
        }

        let _ = std::fs::remove_file(&path);
        Ok(())
    }

    #[test]
    fn test_web_host_defaults_to_all_interfaces_and_is_validated() -> Result<()> {
        let default = router("server_host = \"control.test\"\ntoken = \"secret\"\n");
        assert_eq!(default.web_host(), "0.0.0.0");

        let loopback = router(
            "server_host = \"control.test\"\ntoken = \"secret\"\nweb_host = \"127.0.0.1\"\n",
        );
        assert_eq!(loopback.web_host(), "127.0.0.1");

        let err = Config::parse(
            "server_host = \"control.test\"\ntoken = \"secret\"\nweb_host = \"localhost\"\n",
        )
        .expect_err("a host name must be rejected");
        assert!(
            format!("{:#}", err).contains("web_host"),
            "unexpected error: {:#}",
            err
        );
        Ok(())
    }

    #[test]
    fn test_server_limits_validation() {
        assert!(ServerLimits::default().validate().is_ok());

        let limits = ServerLimits {
            max_control_connections: 0,
            ..ServerLimits::default()
        };
        assert!(limits.validate().is_err());
    }

    /// The number of lines that disable a setting with a single `#`.
    fn disabled_settings(example: &str, prefix: &str) -> usize {
        example
            .lines()
            .filter(|line| line.starts_with(prefix))
            .count()
    }

    /// Check the comment convention of the generated file: `##` marks
    /// documentation and a single `#` only ever disables a setting.
    fn assert_comment_convention(example: &str) {
        for line in example.lines() {
            if line.trim().is_empty() || line.starts_with("##") {
                continue;
            }
            let Some(rest) = line.strip_prefix("# ") else {
                assert!(
                    !line.starts_with('#'),
                    "a comment must start with ##: {:?}",
                    line
                );
                continue;
            };
            assert!(
                rest.contains(" = ") || rest.starts_with("[["),
                "documentation must start with ##, not a single #: {:?}",
                line
            );
        }
    }

    #[test]
    fn test_config_example_is_valid_after_tokens_are_set() -> Result<()> {
        let example = DEFAULT_CONFIG;
        let mut cfg: Config = toml::from_str(example)?;
        assert!(Config::validate(&mut cfg).is_err());
        cfg.token = Some(MaskedString::from("downstream-secret"));
        cfg.upstream.as_mut().unwrap().token = MaskedString::from("upstream-secret");
        Config::validate(&mut cfg)?;
        assert_eq!(cfg.name, "edge");
        assert_eq!(cfg.server_port, 9000);
        assert_eq!(cfg.proxies.len(), 1);
        // The commented standalone section is inactive
        assert_eq!(disabled_settings(example, "# [upstream]"), 0);
        assert_comment_convention(example);
        Ok(())
    }

    const LEAF_CONFIG: &str = r#"
name = "relay-b"
server_port = 9001
server_host = "b.lan"
token = "downstream-secret"

[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"

[upstream]
server = "ws://a.lan:9000"
token = "upstream-secret"
claim = "*.b.relay.example.com"
"#;

    #[test]
    fn test_leaf_config_is_valid() -> Result<()> {
        let config = router(LEAF_CONFIG);
        assert_eq!(config.name, "relay-b");
        assert_eq!(config.server_limits(), ServerLimits::default());
        assert_eq!(config.max_mirrored_routes, 256);
        // The upstream endpoint is canonicalized with an explicit port
        assert_eq!(config.upstream.as_ref().unwrap().server, "ws://a.lan:9000");
        assert_eq!(
            config.upstream.as_ref().unwrap().claim(),
            Some("*.b.relay.example.com")
        );
        assert!(!config.upstream.as_ref().unwrap().mirrors());
        assert_eq!(
            config.upstream.as_ref().unwrap().server_addr().unwrap(),
            "a.lan:9000"
        );
        assert!(!config.upstream.as_ref().unwrap().tls());
        Ok(())
    }

    #[test]
    fn test_upstream_mirror_defaults_on() -> Result<()> {
        let config = router(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"wss://a.lan\"\n\
             token = \"upstream-secret\"\n",
        );
        let upstream = config.upstream.as_ref().unwrap();
        assert!(upstream.mirrors());
        assert_eq!(upstream.mirror, Some(true));
        assert_eq!(upstream.server, "wss://a.lan:443");
        assert!(upstream.tls());
        Ok(())
    }

    #[test]
    fn test_ingress_without_upstream_is_valid() -> Result<()> {
        let config = router(
            "name = \"ingress\"\nserver_port = 9001\nserver_host = \"ingress.lan\"\n\
             token = \"downstream-secret\"\n",
        );
        assert!(config.upstream.is_none());
        Ok(())
    }

    #[test]
    fn test_rejects_an_upstream_pointing_at_itself() {
        let mut config: Config = toml::from_str(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://b.lan:9001\"\n\
             token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n",
        )
        .unwrap();
        assert!(Config::validate(&mut config).is_err());

        for server in [
            "ws://127.0.0.1:9001",
            "ws://localhost:9001",
            "ws://[::1]:9001",
        ] {
            let mut config: Config = toml::from_str(&format!(
                "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
                 token = \"downstream-secret\"\n\n[upstream]\nserver = \"{}\"\n\
                 token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n",
                server
            ))
            .unwrap();
            assert!(
                Config::validate(&mut config).is_err(),
                "{} was accepted",
                server
            );
        }

        // Loopback on another port is a legitimate first hop
        assert!(Config::parse(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://127.0.0.1:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n"
        )
        .is_ok());
    }

    #[test]
    fn test_bind_host_must_cover_loopback_with_upstream() {
        let mut config = router(LEAF_CONFIG);
        config.bind_host = "192.0.2.10".to_string();
        let err =
            Config::validate(&mut config).expect_err("a non-loopback bind_host must be rejected");
        assert!(
            format!("{:#}", err).contains("bind_host"),
            "unexpected error: {:#}",
            err
        );

        for host in ["127.0.0.1", "0.0.0.0", "::1"] {
            let mut config = router(LEAF_CONFIG);
            config.bind_host = host.to_string();
            assert!(
                Config::validate(&mut config).is_ok(),
                "{} was rejected",
                host
            );
        }
    }

    #[test]
    fn test_server_port_zero_requires_upstream() {
        assert!(Config::parse(
            "server_port = 0\nserver_host = \"ingress.lan\"\ntoken = \"downstream-secret\"\n"
        )
        .is_err());

        assert!(Config::parse(
            "server_port = 0\nserver_host = \"leaf.lan\"\ntoken = \"downstream-secret\"\n\n\
             [upstream]\nserver = \"ws://a.lan:9000\"\ntoken = \"upstream-secret\"\n"
        )
        .is_ok());
    }

    #[test]
    fn test_rejects_an_overlapping_claim() {
        // The claim must not contain the reserved host of this process
        assert!(Config::parse(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://a.lan:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"*.lan\"\n"
        )
        .is_err());

        // ... nor the server_host of the next hop, which is the URL host
        assert!(Config::parse(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://a.lan:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"a.lan\"\n"
        )
        .is_err());
    }

    #[test]
    fn test_rejects_invalid_settings() {
        // An empty name is not a useful next-hop identifier
        let mut config = router(LEAF_CONFIG);
        config.name = "  ".to_string();
        assert!(Config::validate(&mut config).is_err());

        // Both tokens are required, and the placeholders are not secrets
        let mut config = router(LEAF_CONFIG);
        config.token = Some(MaskedString::from(""));
        assert!(Config::validate(&mut config).is_err());
        let mut config = router(LEAF_CONFIG);
        config.token = Some(MaskedString::from("replace-with-a-long-random-secret"));
        assert!(Config::validate(&mut config).is_err());
        let mut config = router(LEAF_CONFIG);
        config.upstream.as_mut().unwrap().token = MaskedString::from("");
        assert!(Config::validate(&mut config).is_err());

        // The upstream endpoint must be a WebSocket URL
        let mut config = router(LEAF_CONFIG);
        config.upstream.as_mut().unwrap().server = "a.lan:9000".to_string();
        assert!(Config::validate(&mut config).is_err());

        // The claim is a domain, not a URL or an empty string
        let mut config = router(LEAF_CONFIG);
        config.upstream.as_mut().unwrap().claim = Some("http://a.lan".to_string());
        assert!(Config::validate(&mut config).is_err());

        // Exactly one of `claim` and `mirror` must be set
        let mut config = router(LEAF_CONFIG);
        config.upstream.as_mut().unwrap().mirror = Some(true);
        assert!(Config::validate(&mut config).is_err());
        let mut config = router(LEAF_CONFIG);
        config.upstream.as_mut().unwrap().claim = None;
        config.upstream.as_mut().unwrap().mirror = Some(false);
        assert!(Config::validate(&mut config).is_err());
    }

    #[test]
    fn test_mirror_config_is_valid() -> Result<()> {
        let config = router(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\nmax_mirrored_routes = 0\n\n\
             [upstream]\nserver = \"wss://a.lan\"\n\
             token = \"upstream-secret\"\nmirror = true\n",
        );
        let upstream = config.upstream.as_ref().unwrap();
        assert!(upstream.mirrors());
        assert!(upstream.claim().is_none());
        assert_eq!(upstream.server, "wss://a.lan:443");
        assert_eq!(config.max_mirrored_routes, 0);
        Ok(())
    }
}
