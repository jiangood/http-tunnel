use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use tokio::fs;

/// The interval between retries to connect to the upstream
const DEFAULT_RETRY_INTERVAL_SECS: u64 = 1;

/// The default WebSocket ping interval in secs. Kept well below the usual 60 s
/// idle timeout of a reverse proxy or CDN.
const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// The default listener port of the universal process.
pub const DEFAULT_LISTEN_PORT: u16 = 7777;

/// The default number of unauthenticated WebSocket upgrades a server runs at once.
pub const DEFAULT_MAX_PENDING_HANDSHAKES: usize = 256;
/// The default number of v4 sessions a server accepts at once.
pub const DEFAULT_MAX_SESSIONS: usize = 256;

/// The error returned when the missing configuration file was replaced by the
/// commented example.
///
/// It is a successful bootstrap rather than a failure: the command-line
/// program prints the guidance and exits with status 0, while library callers
/// can still tell it apart from a real load error.
#[derive(Debug)]
pub struct GeneratedConfigError {
    message: String,
}

impl GeneratedConfigError {
    fn new(message: String) -> Self {
        Self { message }
    }
}

impl std::fmt::Display for GeneratedConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for GeneratedConfigError {}

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
/// service of a leaf or relay. A process with `[upstream]` mirrors these
/// domains to its next hop.
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
/// session and mirrors its route set there.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The WebSocket endpoint of the next hop, e.g. `ws://a.lan:7777` or
    /// `wss://relay.example.com`.
    pub url: String,
    /// The shared token of the next hop.
    pub token: MaskedString,
}

impl UpstreamConfig {
    /// The `host:port` authority of the next hop, with an IPv6 host bracketed.
    pub fn addr(&self) -> Option<String> {
        let (_, authority) = split_upstream_url(&self.url)?;
        Some(authority.to_string())
    }

    /// Whether the upstream WebSocket connection uses TLS.
    pub fn tls(&self) -> bool {
        self.url.starts_with("wss://")
    }

    /// The normalized host and port of the next hop.
    pub(crate) fn host_port(&self) -> Result<(String, u16)> {
        parse_upstream_url(&self.url).map(|(_, host, port)| (host, port))
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
    pub(crate) env: crate::config_env::EnvConfig,
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
    #[serde(default = "default_listen_port")]
    pub listen_port: u16,
    /// The exact Host reserved for the control plane: the v4 session endpoint
    /// and the management page/API. It is
    /// required without `[upstream]`, and defaults to `127.0.0.1` with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_host: Option<String>,
    /// The shared token of downstream peers. When omitted with `[upstream]`, the
    /// upstream token is used for the listener too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<MaskedString>,
    /// Local services served by this process.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub proxies: Vec<ServerProxyEntry>,
    /// The next hop. Remove this table to run as a standalone ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamConfig>,
    /// The interval between retries to reach the upstream, in secs.
    #[serde(default = "default_retry_interval")]
    pub retry_interval: u64,
    /// The number of authenticated WebSocket upgrades that may wait for their
    /// `ClientHello` at the same time.
    #[serde(default = "default_max_pending_handshakes")]
    pub max_pending_handshakes: usize,
    /// The number of v4 sessions accepted at once.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
    /// WebSocket ping interval in secs. `0` disables keepalive pings.
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
    /// Load metadata, never written to TOML.
    #[serde(skip)]
    pub(crate) source: ConfigSource,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            name: default_name(),
            listen_port: default_listen_port(),
            control_host: None,
            token: None,
            proxies: Vec::new(),
            upstream: None,
            retry_interval: default_retry_interval(),
            max_pending_handshakes: default_max_pending_handshakes(),
            max_sessions: default_max_sessions(),
            keepalive_interval: default_keepalive_interval(),
            source: ConfigSource::default(),
        }
    }
}

impl Config {
    /// The token the embedded listener authenticates downstream peers with.
    ///
    /// When the top-level `token` is omitted and `[upstream]` is present, the
    /// upstream token is reused for the listener.
    pub fn listener_token(&self) -> Option<&MaskedString> {
        self.token
            .as_ref()
            .or_else(|| self.upstream.as_ref().map(|upstream| &upstream.token))
    }

    /// The resolved `control_host`, which validation always fills in.
    pub fn control_host(&self) -> &str {
        self.control_host
            .as_deref()
            .unwrap_or(crate::helper::CONTROL_HOST_FALLBACK)
    }

    /// The runtime limits of the embedded server.
    pub fn server_limits(&self) -> ServerLimits {
        ServerLimits {
            max_pending_handshakes: self.max_pending_handshakes,
            max_sessions: self.max_sessions,
            keepalive_interval: self.keepalive_interval,
        }
    }

    /// The WebSocket scheme of the upstream, `wss` when there is none.
    pub fn transport_name(&self) -> &str {
        self.upstream
            .as_ref()
            .and_then(|upstream| split_upstream_url(&upstream.url).map(|(scheme, _)| scheme))
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
        self.upstream.as_ref().and_then(UpstreamConfig::addr)
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
        let mut config: Config = match toml::from_str(s) {
            Ok(config) => config,
            Err(err) => {
                if let Some(hint) = legacy_config_hint(s) {
                    return Err(anyhow!(
                        "The configuration uses legacy keys: {}. Rename them and restart. ({})",
                        hint,
                        err
                    ));
                }
                return Err(err).with_context(|| "Failed to parse the config");
            }
        };

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
            Ok(()) => anyhow::Error::new(GeneratedConfigError::new(format!(
                "已生成 {:?}。请修改 `control_host` 和 `token`，客户端或中继再补充 \
                 `[upstream]`，然后重新启动 ht。",
                path
            ))),
            Err(e) => anyhow::Error::new(e)
                .context(format!("Failed to generate default config {:?}", path)),
        }
    }

    /// Validate a configuration and normalize it in place.
    ///
    /// The rules keep the process from becoming an accidental open relay or a
    /// self-loop: the tokens must be set, the local routes must not overlap the
    /// reserved `control_host`, and the upstream must not point back at this
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

        let has_upstream = config.upstream.is_some();
        if config.listen_port == 0 && !has_upstream {
            bail!(
                "`listen_port` may only be 0 when `[upstream]` is present: a standalone \
                 ingress has no upstream endpoint to discover its ephemeral port"
            );
        }

        let mut control_host = match config.control_host.take() {
            Some(host) => host,
            None if has_upstream => crate::helper::CONTROL_HOST_FALLBACK.to_string(),
            None => bail!(
                "`control_host` is required without `[upstream]`: it is the reserved Host of \
                 the v4 session endpoint"
            ),
        };
        let normalized = crate::helper::normalize_domain(&control_host)
            .ok_or_else(|| anyhow!("Invalid control_host"))?;
        if normalized.starts_with("*.") {
            bail!("control_host must be an exact hostname, not a wildcard");
        }
        control_host = normalized;
        config.control_host = Some(control_host.clone());

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
            let local_addr = entry.local_addr.trim();
            if local_addr.is_empty() {
                bail!(
                    "The `local_addr` of the local service `{}` is empty",
                    entry.domain
                );
            }
            // The address is dialed as raw TCP, so a URL scheme such as
            // `http://` is never meaningful and would only fail later, when the
            // first visitor arrives. Reject it while the config is loaded.
            if let Some((scheme, _)) = local_addr.split_once("://") {
                if scheme != "unix" {
                    bail!(
                        "The `local_addr` of the local service `{}` must be `host:port` or \
                         `unix:///path`, not a URL: remove the `{}://` from `{}`",
                        entry.name,
                        scheme,
                        local_addr
                    );
                }
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

            if domain_overlaps_control_host(&domain, &control_host) {
                bail!(
                    "The local service domain `{}` overlaps the reserved control_host `{}`",
                    domain,
                    control_host
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
                local_addr: local_addr.to_string(),
                auth_header: entry.auth_header,
            });
        }
        config.proxies = entries;

        let Some(upstream) = config.upstream.as_mut() else {
            return Ok(());
        };

        let (url, host, port) = canonical_upstream_url(&upstream.url)?;
        upstream.url = url;
        if upstream.token.is_empty() || is_placeholder_token(&upstream.token) {
            bail!("Set a strong, non-placeholder `[upstream].token` for the next hop");
        }

        // A same-port endpoint with the reserved host or a loopback/unspecified
        // address dials this process's own listener. A port of 0 is resolved at
        // startup, after the listener has picked the actual port.
        if config.listen_port != 0
            && port == config.listen_port
            && (host.eq_ignore_ascii_case(&control_host) || is_local_host(&host))
        {
            bail!(
                "`[upstream].url` points at this process's own listener ({}:{}); configure \
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
fn default_listen_port() -> u16 {
    DEFAULT_LISTEN_PORT
}
fn default_retry_interval() -> u64 {
    DEFAULT_RETRY_INTERVAL_SECS
}
fn default_max_pending_handshakes() -> usize {
    DEFAULT_MAX_PENDING_HANDSHAKES
}
fn default_max_sessions() -> usize {
    DEFAULT_MAX_SESSIONS
}
fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_SECS
}

/// The keys that were renamed in 2.0, with their replacement, when the parse
/// failed. It turns a generic `unknown field` error into a migration hint.
fn legacy_config_hint(source: &str) -> Option<String> {
    let value: toml::Value = toml::from_str(source).ok()?;
    let table = value.as_table()?;
    let mut hints = Vec::new();
    for (old, new) in [
        ("server_port", "listen_port"),
        ("server_host", "control_host"),
    ] {
        if table.contains_key(old) {
            hints.push(format!("`{}` -> `{}`", old, new));
        }
    }
    if table.contains_key("bind_host") {
        hints.push("`bind_host` was removed; the listener always binds 0.0.0.0".to_string());
    }
    if table
        .get("upstream")
        .and_then(toml::Value::as_table)
        .is_some_and(|upstream| upstream.contains_key("server"))
    {
        hints.push("`[upstream].server` -> `[upstream].url`".to_string());
    }
    if let Some(upstream) = table.get("upstream").and_then(toml::Value::as_table) {
        for removed in ["mirror", "claim"] {
            if upstream.contains_key(removed) {
                hints.push(format!(
                    "`[upstream].{}` was removed; a process with `[upstream]` always mirrors \
                     its routes",
                    removed
                ));
            }
        }
    }
    if table.contains_key("max_mirrored_routes") {
        hints.push(
            "`max_mirrored_routes` was removed; every route is mirrored without a cap".to_string(),
        );
    }
    if hints.is_empty() {
        None
    } else {
        Some(hints.join(", "))
    }
}

/// Whether a token is one of the documented placeholders rather than a secret.
fn is_placeholder_token(token: &str) -> bool {
    token.starts_with("replace-with-") || token.trim().is_empty()
}

/// Format a host and a port as a `host:port` authority, bracketing IPv6 hosts.
pub(crate) fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Validate the `url` endpoint and store it in a canonical form.
///
/// The canonical form is a lowercase scheme, a host and an explicit, validated
/// port, so the rest of the process can split it without parsing again.
fn canonical_upstream_url(raw: &str) -> Result<(String, String, u16)> {
    let (scheme, host, port) = parse_upstream_url(raw)?;
    Ok((
        format!("{}://{}", scheme, format_host_port(&host, port)),
        host,
        port,
    ))
}

/// Split a canonical `scheme://host:port` endpoint into its scheme and authority.
fn split_upstream_url(upstream_url: &str) -> Option<(&str, &str)> {
    upstream_url.split_once("://")
}

/// Parse a `ws://` or `wss://` endpoint into its scheme, host and port.
///
/// The port is optional and defaults to 443 for `wss` and 80 for `ws`. The
/// path, query and fragment are rejected because the session path is fixed by
/// the protocol.
fn parse_upstream_url(raw: &str) -> Result<(String, String, u16)> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("The `[upstream].url` setting must not be empty");
    }
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        anyhow!(
            "The `[upstream].url` setting must be a WebSocket URL such as \
             \"wss://control.example.com\""
        )
    })?;
    let scheme = scheme.trim().to_ascii_lowercase();
    if scheme != "ws" && scheme != "wss" {
        bail!(
            "The `[upstream].url` scheme must be `ws` or `wss`, not `{}`",
            scheme
        );
    }
    if rest.is_empty() {
        bail!("The `[upstream].url` host is missing");
    }
    // A trailing slash is the root path and is harmless; the session path is
    // appended by the protocol itself.
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains('/') || rest.contains('\\') || rest.contains('?') || rest.contains('#') {
        bail!(
            "The `[upstream].url` setting must contain only a host and an optional port, \
             without a path, query or fragment"
        );
    }

    let default_port = if scheme == "wss" { 443 } else { 80 };
    let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| {
            anyhow!(
                "The `[upstream].url` host `{}` is not a valid IPv6 address",
                raw
            )
        })?;
        let port = if tail.is_empty() {
            default_port
        } else if let Some(port) = tail.strip_prefix(':') {
            port.parse::<u16>()
                .map_err(|_| anyhow!("The `[upstream].url` port `{}` is not a valid port", port))?
        } else {
            bail!("The `[upstream].url` host `{}` is invalid", raw);
        };
        (format!("[{}]", host), port)
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                let port = port.parse::<u16>().map_err(|_| {
                    anyhow!("The `[upstream].url` port `{}` is not a valid port", port)
                })?;
                (host.to_string(), port)
            }
            Some(_) => {
                bail!(
                    "The IPv6 host of `[upstream].url` must be bracketed, e.g. \"wss://[::1]:443\""
                )
            }
            None => (rest.to_string(), default_port),
        }
    };
    let host = normalize_upstream_host(&host)?;
    if port == 0 {
        bail!("The `[upstream].url` port must not be 0");
    }
    Ok((scheme, host, port))
}

/// Normalize and validate the host of the upstream URL.
///
/// A host is a DNS name, an IPv4 address or an IPv6 address. The port belongs to
/// `[upstream].url`, so a `host:port` value is rejected with a hint. IPv6
/// literals may be written with or without brackets.
fn normalize_upstream_host(raw: &str) -> Result<String> {
    let host = raw.trim();
    if host.is_empty() {
        bail!("The `[upstream].url` host must not be empty");
    }
    if host.contains("://") {
        bail!("The `[upstream].url` host must not include a scheme");
    }
    if host.chars().any(|c| c.is_whitespace()) {
        bail!("The `[upstream].url` host must not contain whitespace");
    }
    if host.contains('/')
        || host.contains('\\')
        || host.contains('?')
        || host.contains('#')
        || host.contains('@')
    {
        bail!("The upstream host `{}` is not a valid host", raw);
    }

    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The `[upstream].url` host `{}` is not a valid IPv6 address",
                raw
            )
        })?;
        return Ok(inner.to_string());
    }
    if host.contains('[') || host.contains(']') {
        bail!("The upstream host `{}` is not a valid host", raw);
    }
    if host.contains(':') {
        host.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The upstream host `{}` must not include a port; put the port in \
                 `[upstream].url` instead",
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
/// `control_host`. A wildcard overlaps every host below its suffix.
pub(crate) fn domain_overlaps_control_host(domain: &str, control_host: &str) -> bool {
    domain == control_host
        || domain.strip_prefix("*.").is_some_and(|suffix| {
            control_host.len() > suffix.len() && control_host.ends_with(&format!(".{}", suffix))
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

/// The runtime limits of the embedded server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLimits {
    pub max_pending_handshakes: usize,
    pub max_sessions: usize,
    pub keepalive_interval: u64,
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_pending_handshakes: DEFAULT_MAX_PENDING_HANDSHAKES,
            max_sessions: DEFAULT_MAX_SESSIONS,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL_SECS,
        }
    }
}

impl ServerLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pending_handshakes == 0 || self.max_sessions == 0 {
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
    fn test_upstream_url_validation() -> Result<()> {
        let ws = router(
            "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             url = \"ws://example.com\"\ntoken = \"secret\"\n",
        );
        assert_eq!(ws.upstream.as_ref().unwrap().url, "ws://example.com:80");
        assert_eq!(ws.transport_name(), "ws");
        assert!(!ws.tls());

        assert!(Config::parse(
            "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             url = \"udp://example.com\"\ntoken = \"secret\"\n"
        )
        .is_err());
        assert!(Config::parse(
            "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             url = \"example.com:443\"\ntoken = \"secret\"\n"
        )
        .is_err());
        Ok(())
    }

    #[test]
    fn test_upstream_addr() -> Result<()> {
        let cfg = router(
            "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             url = \"wss://example.com:2333\"\ntoken = \"secret\"\n",
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().addr().as_deref(),
            Some("example.com:2333")
        );

        let cfg = router(
            "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
             url = \"wss://[::1]\"\ntoken = \"secret\"\n",
        );
        assert_eq!(
            cfg.upstream.as_ref().unwrap().addr().as_deref(),
            Some("[::1]:443")
        );
        Ok(())
    }

    #[test]
    fn test_rejects_an_invalid_upstream_address() {
        for url in [
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
                "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
                 url = {:?}\ntoken = \"secret\"\n",
                url
            );
            assert!(Config::parse(&source).is_err(), "{} is valid upstream", url);
        }
    }

    #[test]
    fn test_legacy_config_keys_are_rejected_with_a_hint() {
        for (source, old, new) in [
            (
                "server_port = 7777\ncontrol_host = \"control.test\"\ntoken = \"secret\"\n",
                "server_port",
                "listen_port",
            ),
            (
                "server_host = \"control.test\"\ntoken = \"secret\"\n",
                "server_host",
                "control_host",
            ),
            (
                "control_host = \"control.test\"\ntoken = \"secret\"\n\n[upstream]\n\
                 server = \"ws://a.test:7777\"\ntoken = \"secret\"\n",
                "[upstream].server",
                "[upstream].url",
            ),
            (
                "bind_host = \"127.0.0.1\"\ncontrol_host = \"control.test\"\n\
                 token = \"secret\"\n",
                "bind_host",
                "removed",
            ),
        ] {
            let err = format!("{:#}", Config::parse(source).unwrap_err());
            assert!(err.contains("legacy keys"), "{}", err);
            assert!(err.contains(old), "{}", err);
            assert!(err.contains(new), "{}", err);
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
control_host = "control.test"
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
    fn test_set_and_remove_proxy() -> Result<()> {
        let mut cfg = router("control_host = \"control.test\"\ntoken = \"secret\"\n");

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
        let mut cfg = router("control_host = \"control.test\"\ntoken = \"secret\"\n");

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
        assert_eq!(cfg.listen_port, DEFAULT_LISTEN_PORT);
        assert_eq!(cfg.retry_interval, DEFAULT_RETRY_INTERVAL_SECS);
        assert!(cfg.proxies.is_empty());
        Ok(())
    }

    #[test]
    fn test_token_falls_back_to_the_upstream_token() -> Result<()> {
        let cfg = router(
            "control_host = \"control.test\"\n\n[upstream]\nurl = \"ws://a.test:7777\"\n\
             token = \"upstream-secret\"\n",
        );
        assert!(cfg.token.is_none());
        assert_eq!(&**cfg.listener_token().unwrap(), "upstream-secret");
        let dumped = cfg.to_toml()?;
        assert!(
            !dumped.contains("token = \"upstream-secret\"\ncontrol_host"),
            "{}",
            dumped
        );
        Ok(())
    }

    #[test]
    fn test_rejects_an_empty_local_addr() {
        let s = r#"
control_host = "control.test"
token = "secret"

[[proxies]]
name = "foo"
domain = "foo.example.com"
local_addr = ""
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_url_scheme_in_local_addr() {
        for local_addr in [
            "http://127.0.0.1:80",
            "https://nas.example.com",
            "tcp://127.0.0.1:80",
            "HTTPS://nas.example.com",
        ] {
            let s = format!(
                "control_host = \"control.test\"\ntoken = \"secret\"\n\n[[proxies]]\n\
                 name = \"foo\"\ndomain = \"foo.example.com\"\nlocal_addr = {:?}\n",
                local_addr
            );
            let error = Config::parse(&s).expect_err(local_addr);
            let message = format!("{:#}", error);
            assert!(
                message.contains("must be `host:port` or `unix:///path`"),
                "{}",
                message
            );
        }
    }

    #[test]
    fn test_accepts_a_unix_socket_and_trims_local_addr() -> Result<()> {
        let s = r#"
control_host = "control.test"
token = "secret"

[[proxies]]
name = "docker"
domain = "docker.example.com"
local_addr = "unix:///var/run/docker.sock"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "  127.0.0.1:80  "
"#;
        let config = Config::parse(s)?;
        assert_eq!(config.proxies[0].local_addr, "unix:///var/run/docker.sock");
        assert_eq!(config.proxies[1].local_addr, "127.0.0.1:80");
        Ok(())
    }

    #[test]
    fn test_rejects_an_invalid_domain() {
        let s = r#"
control_host = "control.test"
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
control_host = "control.test"
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
control_host = "control.test"
token = "secret"
tokenz = "oops"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_name() {
        let s = r#"
control_host = "control.test"
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
control_host = "control.test"
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
control_host = "control.test"
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
control_host = "control.test"
token = "secret"

[[proxies]]
name = "nas/../etc"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#;
        assert!(Config::parse(s).is_err());
    }

    #[test]
    fn test_config_roundtrip() -> Result<()> {
        let s = r#"
name = "edge"
listen_port = 7777
control_host = "control.test"
token = "secret"
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
url = "ws://a.test:7777"
token = "upstream-secret"
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
control_host = "control.test"
token = "secret"

[[proxies]]
name = "foo1"
domain = "foo1.example.com"
local_addr = "127.0.0.1:80"
"#;
        let cfg = router(s);
        let dumped = cfg.to_toml()?;
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
    fn test_domain_cannot_overlap_control_host() {
        assert!(domain_overlaps_control_host(
            "control.example.com",
            "control.example.com"
        ));
        assert!(domain_overlaps_control_host(
            "*.example.com",
            "control.example.com"
        ));
        assert!(!domain_overlaps_control_host(
            "*.example.com",
            "example.com"
        ));
        assert!(!domain_overlaps_control_host(
            "*.other.test",
            "control.example.com"
        ));
    }

    #[tokio::test]
    async fn test_control_host_defaults_to_loopback_with_upstream() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-control-host-{}.toml",
            std::process::id()
        ));

        // Without upstream, the reserved Host is required.
        std::fs::write(&path, "listen_port = 8080\ntoken = \"a_secret_token\"\n")?;
        let err = Config::from_file(&path)
            .await
            .expect_err("a missing control_host must be rejected");
        assert!(
            format!("{:#}", err).contains("control_host"),
            "unexpected error: {:#}",
            err
        );

        // With upstream it defaults to loopback.
        std::fs::write(
            &path,
            "listen_port = 8080\ntoken = \"a_secret_token\"\n\n[upstream]\n\
             url = \"ws://a.test:7777\"\ntoken = \"upstream-secret\"\n",
        )?;
        let cfg = Config::from_file(&path).await?;
        assert_eq!(cfg.control_host(), "127.0.0.1");

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
    async fn test_bind_host_is_not_a_config_field() {
        let path =
            std::env::temp_dir().join(format!("http-tunnel-bind-host-{}.toml", std::process::id()));
        let base = "listen_port = 8080\ncontrol_host = \"control.example.com\"\n\
                    token = \"a_secret_token\"\n";
        std::fs::write(&path, format!("{}bind_host = \"127.0.0.1\"\n", base)).unwrap();
        let err = Config::from_file(&path)
            .await
            .expect_err("bind_host was removed from the configuration");
        assert!(
            format!("{:#}", err).contains("bind_host"),
            "unexpected error: {:#}",
            err
        );

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_server_limits_validation() {
        assert!(ServerLimits::default().validate().is_ok());

        let limits = ServerLimits {
            max_sessions: 0,
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
        assert_eq!(cfg.listen_port, 7777);
        assert_eq!(cfg.proxies.len(), 1);
        // The commented standalone section is inactive
        assert_eq!(disabled_settings(example, "# [upstream]"), 0);
        assert_comment_convention(example);
        Ok(())
    }

    const LEAF_CONFIG: &str = r#"
name = "relay-b"
listen_port = 9001
control_host = "b.lan"
token = "downstream-secret"

[[proxies]]
name = "local"
domain = "local.b.relay.example.com"
local_addr = "127.0.0.1:8080"

[upstream]
url = "ws://a.lan:7777"
token = "upstream-secret"
"#;

    #[test]
    fn test_leaf_config_is_valid() -> Result<()> {
        let config = router(LEAF_CONFIG);
        assert_eq!(config.name, "relay-b");
        assert_eq!(config.server_limits(), ServerLimits::default());
        // The upstream endpoint is canonicalized with an explicit port
        assert_eq!(config.upstream.as_ref().unwrap().url, "ws://a.lan:7777");
        assert_eq!(
            config.upstream.as_ref().unwrap().addr().unwrap(),
            "a.lan:7777"
        );
        assert!(!config.upstream.as_ref().unwrap().tls());
        Ok(())
    }

    #[test]
    fn test_upstream_url_gets_its_defaults() -> Result<()> {
        let config = router(
            "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nurl = \"wss://a.lan\"\n\
             token = \"upstream-secret\"\n",
        );
        let upstream = config.upstream.as_ref().unwrap();
        assert_eq!(upstream.url, "wss://a.lan:443");
        assert!(upstream.tls());
        Ok(())
    }

    #[test]
    fn test_ingress_without_upstream_is_valid() -> Result<()> {
        let config = router(
            "name = \"ingress\"\nlisten_port = 9001\ncontrol_host = \"ingress.lan\"\n\
             token = \"downstream-secret\"\n",
        );
        assert!(config.upstream.is_none());
        Ok(())
    }

    #[test]
    fn test_rejects_an_upstream_pointing_at_itself() {
        let mut config: Config = toml::from_str(
            "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nurl = \"ws://b.lan:9001\"\n\
             token = \"upstream-secret\"\n",
        )
        .unwrap();
        assert!(Config::validate(&mut config).is_err());

        for url in [
            "ws://127.0.0.1:9001",
            "ws://localhost:9001",
            "ws://[::1]:9001",
        ] {
            let mut config: Config = toml::from_str(&format!(
                "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
                 token = \"downstream-secret\"\n\n[upstream]\nurl = \"{}\"\n\
                 token = \"upstream-secret\"\n",
                url
            ))
            .unwrap();
            assert!(
                Config::validate(&mut config).is_err(),
                "{} was accepted",
                url
            );
        }

        // Loopback on another port is a legitimate first hop
        assert!(Config::parse(
            "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nurl = \"ws://127.0.0.1:7777\"\n\
             token = \"upstream-secret\"\n"
        )
        .is_ok());
    }

    #[test]
    fn test_listen_port_zero_requires_upstream() {
        assert!(Config::parse(
            "listen_port = 0\ncontrol_host = \"ingress.lan\"\ntoken = \"downstream-secret\"\n"
        )
        .is_err());

        assert!(Config::parse(
            "listen_port = 0\ncontrol_host = \"leaf.lan\"\ntoken = \"downstream-secret\"\n\n\
             [upstream]\nurl = \"ws://a.lan:7777\"\ntoken = \"upstream-secret\"\n"
        )
        .is_ok());
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
        config.upstream.as_mut().unwrap().url = "a.lan:7777".to_string();
        assert!(Config::validate(&mut config).is_err());
    }

    #[test]
    fn test_rejects_removed_upstream_keys() {
        // Route advertisement always mirrors the local and downstream routes.
        // The old switches are gone, and a stale configuration must not start
        // with a silently different topology.
        for stale in [
            "mirror = true",
            "mirror = false",
            "claim = \"*.b.relay.example.com\"",
        ] {
            let s = format!(
                "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
                 token = \"downstream-secret\"\n\n[upstream]\nurl = \"ws://a.lan:7777\"\n\
                 token = \"upstream-secret\"\n{}\n",
                stale
            );
            let err = format!("{:#}", Config::parse(&s).unwrap_err());
            assert!(err.contains("legacy keys"), "{}: {}", stale, err);
            assert!(err.contains("always mirrors"), "{}: {}", stale, err);
        }

        let err = format!(
            "{:#}",
            Config::parse(
                "name = \"b\"\nlisten_port = 9001\ncontrol_host = \"b.lan\"\n\
                 token = \"downstream-secret\"\nmax_mirrored_routes = 0\n\n\
                 [upstream]\nurl = \"ws://a.lan:7777\"\ntoken = \"upstream-secret\"\n",
            )
            .unwrap_err()
        );
        assert!(err.contains("max_mirrored_routes"), "{}", err);
        assert!(err.contains("without a cap"), "{}", err);
    }
}
