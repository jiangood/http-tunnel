use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::Path;
use tokio::fs;

/// Application-layer heartbeat timeout in secs
const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 40;

/// The interval between retries to connect to the server
const DEFAULT_CLIENT_RETRY_INTERVAL_SECS: u64 = 1;

/// The default WebSocket ping interval in secs. Kept well below the usual 60 s
/// idle timeout of a reverse proxy or CDN.
const DEFAULT_KEEPALIVE_INTERVAL_SECS: u64 = 15;

/// The default number of unauthenticated WebSocket upgrades a server runs at once.
pub const DEFAULT_MAX_PENDING_HANDSHAKES: usize = 256;
/// The default number of control channels a server accepts at once.
pub const DEFAULT_MAX_CONTROL_CONNECTIONS: usize = 256;
/// The default number of data channels a server accepts at once.
pub const DEFAULT_MAX_DATA_CONNECTIONS: usize = 2048;
/// The default number of warm data channels kept per tunnel.
pub const DEFAULT_POOL_MIN: usize = 4;
/// The default upper bound of the warm data channels of one tunnel.
pub const DEFAULT_POOL_MAX: usize = 32;
/// The default cap on the routes one node mirrors to its next hop. It matches
/// the default control-channel limit of a server.
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

/// A tunnel as it runs on the client.
///
/// The client derives it from the `[[tunnels]]` array of its configuration file.
/// A tunnel has two names: the `name`, its identity within the client, which the
/// administration API uses to address it; and the `domain`, the `Host` header it
/// serves, which the client announces on the control channel so that the server
/// can route to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTunnelConfig {
    /// The identity of the tunnel, unique within the client
    pub name: String,
    /// The domain (the `Host` header) that is routed to this tunnel
    pub domain: String,
    /// The address of the local service on the client side
    pub local_addr: String,
    pub auth_header: Option<MaskedString>,
    /// Deprecated: WebSocket sockets always use TCP_NODELAY. Kept so that old
    /// configurations stay valid.
    pub nodelay: bool,
    /// The interval between retries to connect to the server
    pub retry_interval: u64,
}

/// A tunnel as declared in the `[[tunnels]]` array of the configuration file.
///
/// The `name` is the required identity of the tunnel within the client and is what the
/// administration API addresses.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TunnelEntry {
    /// The required identity of the tunnel, unique within the client
    pub name: String,
    /// The domain (the `Host` header) that is routed to this tunnel
    pub domain: String,
    /// The address of the local service on the client side
    pub local_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<MaskedString>,
}

/// The configuration of a client. The TOML file is the only source of runtime configuration.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// The WebSocket endpoint of the server, e.g. `wss://control.example.com`
    /// or `ws://127.0.0.1:8080`. The scheme selects TLS and the port defaults
    /// to 443 for `wss` and 80 for `ws`. The host must be the `server_host`
    /// of the server and the port its `server_port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// The name of the client. It's used for logging and to warn when a domain
    /// changes hands
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The token of the client. It must match the shared token of the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<MaskedString>,
    /// The port of the client web management page and API. Defaults to 8610, `0` disables it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_port: Option<u16>,
    /// The address the web management page and API bind. Defaults to `0.0.0.0`;
    /// set it to `127.0.0.1` to keep it on this machine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_host: Option<String>,
    /// Application-layer heartbeat timeout in secs. 0 disables the test
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout: Option<u64>,
    /// WebSocket ping interval in secs. 0 disables the keepalive pings
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_interval: Option<u64>,
    /// The interval between retries to connect to the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_interval: Option<u64>,
    /// Deprecated: WebSocket sockets always use TCP_NODELAY. Accepted for
    /// compatibility with configurations written for the TCP transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodelay: Option<bool>,
    /// The tunnels declared by the client
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<TunnelEntry>,
}

impl ClientConfig {
    /// The address the web management page and API bind.
    pub fn web_host(&self) -> &str {
        self.web_host
            .as_deref()
            .unwrap_or(crate::helper::DEFAULT_BIND_HOST)
    }

    /// The `host:port` authority of the server, with an IPv6 host bracketed.
    pub fn server_addr(&self) -> Option<String> {
        let server = self.server.as_deref()?;
        let (_, authority) = split_server_url(server)?;
        Some(authority.to_string())
    }

    /// The interval between two application-layer heartbeats expected from the
    /// server. `0` disables the test
    pub fn heartbeat_timeout(&self) -> u64 {
        self.heartbeat_timeout
            .unwrap_or(DEFAULT_HEARTBEAT_TIMEOUT_SECS)
    }

    /// The interval between retries to connect to the server
    pub fn retry_interval(&self) -> u64 {
        self.retry_interval
            .unwrap_or(DEFAULT_CLIENT_RETRY_INTERVAL_SECS)
    }

    /// Whether the tunnels of this client enable TCP_NODELAY
    pub fn nodelay(&self) -> bool {
        self.nodelay.unwrap_or(true)
    }

    /// The WebSocket scheme, `wss` unless `server` says `ws`.
    pub fn transport_name(&self) -> &str {
        self.server
            .as_deref()
            .and_then(|server| split_server_url(server).map(|(scheme, _)| scheme))
            .unwrap_or("wss")
    }

    /// Whether the WebSocket connection uses TLS. TLS is the default.
    pub fn tls(&self) -> bool {
        self.transport_name() == "wss"
    }

    /// The WebSocket ping interval in secs, `0` meaning disabled
    pub fn keepalive_interval(&self) -> u64 {
        self.keepalive_interval
            .unwrap_or(DEFAULT_KEEPALIVE_INTERVAL_SECS)
    }

    /// The resolved tunnels, sorted by name for a stable order
    pub fn resolved_tunnels(&self) -> Vec<ClientTunnelConfig> {
        let retry_interval = self.retry_interval();
        let nodelay = self.nodelay();
        let mut tunnels: Vec<ClientTunnelConfig> = self
            .tunnels
            .iter()
            .map(|entry| ClientTunnelConfig {
                name: entry.name.clone(),
                domain: entry.domain.clone(),
                local_addr: entry.local_addr.clone(),
                auth_header: entry.auth_header.clone(),
                nodelay,
                retry_interval,
            })
            .collect();
        tunnels.sort_by(|a, b| a.name.cmp(&b.name));
        tunnels
    }

    /// The name of a declared tunnel
    fn entry_name(entry: &TunnelEntry) -> &str {
        &entry.name
    }

    /// Replace the tunnel of the given name, or add it
    pub fn set_tunnel(&mut self, name: String, domain: String, local_addr: String) {
        match self
            .tunnels
            .iter_mut()
            .find(|e| Self::entry_name(e) == name.as_str())
        {
            Some(entry) => {
                entry.domain = domain;
                entry.local_addr = local_addr;
            }
            None => self.tunnels.push(TunnelEntry {
                name,
                domain,
                local_addr,
                auth_header: None,
            }),
        }
    }

    /// Remove the tunnel of the given name. Returns whether one was removed.
    pub fn remove_tunnel(&mut self, name: &str) -> bool {
        let before = self.tunnels.len();
        self.tunnels.retain(|e| Self::entry_name(e) != name);
        self.tunnels.len() != before
    }

    /// Parse a configuration and normalize it
    pub fn parse(s: &str) -> Result<ClientConfig> {
        let mut config: ClientConfig = toml::from_str(s)
            .map_err(|e| {
                if has_legacy_remote(s) {
                    anyhow!(
                        "{}\nNote: `remote` is no longer supported. Set `server = \
                         \"wss://host:port\"` instead.",
                        e
                    )
                } else if s.contains("http_port")
                    || s.contains("control_host")
                    || s.contains("[clients")
                    || s.contains("[[proxies")
                    || s.contains("[proxies")
                {
                    anyhow!(
                        "{}\nNote: `http-tunnel` no longer takes a server configuration file. \
                         Configure the server in `hts.toml`; client routes belong in \
                         the `[[tunnels]]` array of `htc.toml`.",
                        e
                    )
                } else if has_legacy_server_settings(s) {
                    anyhow!(
                        "{}\nNote: `transport`, `server_host` and `server_port` are no longer \
                         supported. Set `server = \"wss://host:port\"` instead (use `ws://` only \
                         on a trusted network).",
                        e
                    )
                } else {
                    anyhow!("{}", e)
                }
            })
            .with_context(|| "Failed to parse the config")?;

        ClientConfig::validate(&mut config)?;

        Ok(config)
    }

    /// Read the fixed client config. If it is missing, create a commented
    /// example and ask the user to edit it before starting.
    pub async fn from_file(path: &Path) -> Result<ClientConfig> {
        match ClientConfig::read(path).await? {
            Some(s) => ClientConfig::parse(&s)
                .with_context(|| format!("The configuration {:?} is invalid", path)),
            None => Err(ClientConfig::bootstrap_example(path).await),
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
        match fs::write(path, DEFAULT_CLIENT_CONFIG).await {
            Ok(()) => anyhow!(
                "Generated {:?}. Edit it with your server address, shared token and tunnel settings, then restart htc.",
                path
            ),
            Err(e) => anyhow::Error::new(e)
                .context(format!("Failed to generate default config {:?}", path)),
        }
    }

    /// Validate a configuration and normalize it in place: validate and
    /// canonicalize the WebSocket endpoint, lowercase the tunnel domains,
    /// validate required names, and reject empty, malformed or duplicated
    /// definitions.
    pub fn validate(config: &mut ClientConfig) -> Result<()> {
        normalize_server(config)?;
        if let Some(web_host) = config.web_host.as_mut() {
            normalize_bind_host(web_host, "web_host")?;
        }
        let mut names: BTreeMap<String, ()> = BTreeMap::new();
        let mut domains: BTreeMap<String, ()> = BTreeMap::new();
        let mut entries: Vec<TunnelEntry> = Vec::new();

        for entry in std::mem::take(&mut config.tunnels) {
            if entry.local_addr.is_empty() {
                bail!("The `local_addr` of the tunnel `{}` is empty", entry.domain);
            }
            if entry
                .auth_header
                .as_ref()
                .is_some_and(|v| v.contains('\r') || v.contains('\n'))
            {
                bail!(
                    "The `auth_header` of the tunnel `{}` must not contain line breaks",
                    entry.name
                );
            }

            let domain = crate::helper::normalize_domain(&entry.domain)
                .ok_or_else(|| anyhow!("The domain `{}` is invalid", entry.domain))?;

            let name = validate_name(&entry.name)?;

            if domains.insert(domain.clone(), ()).is_some() {
                bail!("The domain `{}` is defined more than once", domain);
            }
            if names.insert(name.clone(), ()).is_some() {
                bail!("The tunnel name `{}` is defined more than once", name);
            }

            entries.push(TunnelEntry {
                name,
                domain,
                local_addr: entry.local_addr,
                auth_header: entry.auth_header,
            });
        }

        config.tunnels = entries;

        Ok(())
    }

    /// Serialize the configuration back to TOML
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).with_context(|| "Failed to serialize the config")
    }

    /// Write the configuration back to the file atomically, so that a change made
    /// through the client administration API survives a restart
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

/// Format a host and a port as a `host:port` authority, bracketing IPv6 hosts.
pub(crate) fn format_server_addr(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{}]:{}", host, port)
    } else {
        format!("{}:{}", host, port)
    }
}

/// Whether the TOML source declares a top-level setting with the given key.
fn has_setting(s: &str, key: &str) -> bool {
    s.lines().any(|line| {
        line.trim_start()
            .strip_prefix(key)
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Whether the TOML source still declares the removed top-level `remote` key.
fn has_legacy_remote(s: &str) -> bool {
    has_setting(s, "remote")
}

/// Whether the TOML source still declares any of the settings that `server`
/// replaced.
fn has_legacy_server_settings(s: &str) -> bool {
    has_setting(s, "transport") || has_setting(s, "server_host") || has_setting(s, "server_port")
}

/// Validate the `server` endpoint and store it in a canonical form.
///
/// The canonical form is a lowercase scheme, a host and an explicit, validated
/// port, so the rest of the client can split it without parsing again.
fn normalize_server(config: &mut ClientConfig) -> Result<()> {
    if let Some(server) = config.server.as_mut() {
        *server = canonical_server_url(server)?.0;
    }
    Ok(())
}

/// Validate a `ws://` or `wss://` endpoint and return it in canonical form with
/// an explicit port, together with the normalized host and the port.
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
        bail!("The `server` setting must not be empty");
    }
    let (scheme, rest) = raw.split_once("://").ok_or_else(|| {
        anyhow!(
            "The `server` setting must be a WebSocket URL such as \
             \"wss://control.example.com\""
        )
    })?;
    let scheme = scheme.trim().to_ascii_lowercase();
    if scheme != "ws" && scheme != "wss" {
        bail!(
            "The `server` scheme must be `ws` or `wss`, not `{}`",
            scheme
        );
    }
    if rest.is_empty() {
        bail!("The `server` host is missing");
    }
    // A trailing slash is the root path and is harmless; the control path is
    // appended by the protocol itself.
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.contains('/') || rest.contains('\\') || rest.contains('?') || rest.contains('#') {
        bail!(
            "The `server` setting must contain only a host and an optional port, \
             without a path, query or fragment"
        );
    }

    let default_port = if scheme == "wss" { 443 } else { 80 };
    let (host, port) = if let Some(rest) = rest.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("The `server` host `{}` is not a valid IPv6 address", raw))?;
        let port = if tail.is_empty() {
            default_port
        } else if let Some(port) = tail.strip_prefix(':') {
            port.parse::<u16>()
                .map_err(|_| anyhow!("The `server` port `{}` is not a valid port", port))?
        } else {
            bail!("The `server` host `{}` is invalid", raw);
        };
        (format!("[{}]", host), port)
    } else {
        match rest.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                let port = port
                    .parse::<u16>()
                    .map_err(|_| anyhow!("The `server` port `{}` is not a valid port", port))?;
                (host.to_string(), port)
            }
            Some(_) => {
                bail!("The IPv6 host of `server` must be bracketed, e.g. \"wss://[::1]:443\"")
            }
            None => (rest.to_string(), default_port),
        }
    };
    let host = normalize_server_host(&host)?;
    if port == 0 {
        bail!("The `server` port must not be 0");
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
        bail!("The `server` host must not be empty");
    }
    if host.contains("://") {
        bail!("The `server` host must not include a scheme; put the scheme in `server`");
    }
    if host.chars().any(|c| c.is_whitespace()) {
        bail!("The `server` host must not contain whitespace");
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
        inner
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| anyhow!("The server host `{}` is not a valid IPv6 address", raw))?;
        return Ok(inner.to_string());
    }
    if host.contains('[') || host.contains(']') {
        bail!("The server host `{}` is not a valid host", raw);
    }
    if host.contains(':') {
        host.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The server host `{}` must not include a port; put the port in `server` instead",
                raw
            )
        })?;
    }
    Ok(host.to_string())
}

/// Validate a tunnel name and return it trimmed.
///
/// A name is used in the path of the administration API, so it is restricted to
/// a URL-safe alphabet. The domains and the names share no namespace: a name may
/// look like a domain.
fn validate_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("A tunnel `name` must not be empty");
    }
    let valid = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
    if !valid {
        bail!(
            "The tunnel name `{}` is invalid: only letters, digits, dot, \
             underscore and hyphen are allowed",
            name
        );
    }
    Ok(name.to_string())
}

/// The example written when htc.toml is missing.
///
/// `examples/minimal/htc.toml` is the single source of truth: it is embedded at
/// compile time, so the shipped binary needs no example directory at runtime.
/// The tests in this module keep the example a valid configuration.
pub(crate) const DEFAULT_CLIENT_CONFIG: &str = include_str!("../examples/minimal/htc.toml");

/// The example written when hts.toml is missing.
///
/// The shared token is a placeholder on purpose: it must be strong and match the
/// one in htc.toml, which only the operator can decide.
pub(crate) const DEFAULT_SERVER_CONFIG: &str = include_str!("../examples/minimal/hts.toml");

/// The example written when htn.toml is missing.
///
/// Both tokens are placeholders on purpose: a node has a token for its
/// downstream peers and a different one for its upstream hop.
pub(crate) const DEFAULT_NODE_CONFIG: &str = include_str!("../examples/minimal/htn.toml");

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

    #[test]
    fn test_server_url_validation() -> Result<()> {
        let ws = ClientConfig::parse("server = \"ws://example.com\"\n")?;
        assert_eq!(ws.server.as_deref(), Some("ws://example.com:80"));
        assert_eq!(ws.transport_name(), "ws");
        assert!(!ws.tls());

        assert!(ClientConfig::parse("server = \"udp://example.com\"\n").is_err());
        assert!(ClientConfig::parse("server = \"tls://example.com\"\n").is_err());
        assert!(ClientConfig::parse("server = \"example.com:443\"\n").is_err());

        // TLS is the default, and the address is required at startup
        let default = ClientConfig::parse("")?;
        assert_eq!(default.transport_name(), "wss");
        assert_eq!(default.server, None);
        assert!(default.tls());
        Ok(())
    }

    #[test]
    fn test_ws_and_wss_transports() -> Result<()> {
        let ws = ClientConfig::parse("server = \"WS://example.com\"\n")?;
        assert_eq!(ws.server.as_deref(), Some("ws://example.com:80"));
        assert_eq!(ws.transport_name(), "ws");
        assert!(!ws.tls());

        let wss = ClientConfig::parse("server = \"wss://example.com\"\n")?;
        assert_eq!(wss.server.as_deref(), Some("wss://example.com:443"));
        assert_eq!(wss.transport_name(), "wss");
        assert!(wss.tls());

        // A trailing root slash is accepted and normalized away
        let slash = ClientConfig::parse("server = \"wss://example.com/\"\n")?;
        assert_eq!(slash.server.as_deref(), Some("wss://example.com:443"));

        assert_eq!(ws.keepalive_interval(), DEFAULT_KEEPALIVE_INTERVAL_SECS);
        let off = ClientConfig::parse("keepalive_interval = 0\n")?;
        assert_eq!(off.keepalive_interval(), 0);
        Ok(())
    }

    #[test]
    fn test_server_addr() -> Result<()> {
        let cfg = ClientConfig::parse("server = \"wss://example.com:2333\"\n")?;
        assert_eq!(cfg.server_addr().as_deref(), Some("example.com:2333"));

        let cfg = ClientConfig::parse("server = \"wss://[::1]:2333\"\n")?;
        assert_eq!(cfg.server_addr().as_deref(), Some("[::1]:2333"));

        // An omitted port defaults to the scheme
        let cfg = ClientConfig::parse("server = \"wss://[::1]\"\n")?;
        assert_eq!(cfg.server_addr().as_deref(), Some("[::1]:443"));
        let cfg = ClientConfig::parse("server = \"ws://[::1]\"\n")?;
        assert_eq!(cfg.server_addr().as_deref(), Some("[::1]:80"));
        Ok(())
    }

    #[test]
    fn test_rejects_the_legacy_server_settings() {
        for source in [
            "transport = \"ws\"\nserver_host = \"example.com\"\nserver_port = 2333\n",
            "transport = \"tcp\"\n",
            "server_host = \"example.com\"\n",
            "server_port = 2333\n",
            "server = \"wss://example.com\"\nserver_host = \"example.com\"\n\
             server_port = 443\n",
        ] {
            let err = format!("{:#}", ClientConfig::parse(source).unwrap_err());
            assert!(
                err.contains("no longer supported"),
                "{} did not report the migration: {}",
                source,
                err
            );
            assert!(err.contains("server = \"wss://host:port\""), "{}", err);
        }
    }

    #[test]
    fn test_rejects_an_invalid_server_address() {
        for source in [
            "server = \"example.com:2333\"\n",
            "server = \"tcp://example.com\"\n",
            "server = \"\"\n",
            "server = \"wss://\"\n",
            "server = \"wss://example.com:0\"\n",
            "server = \"wss://example.com/path\"\n",
            "server = \"wss://::1:443\"\n",
            "server = \"wss://user@example.com\"\n",
        ] {
            assert!(ClientConfig::parse(source).is_err(), "{} is valid", source);
        }
    }

    #[test]
    fn test_rejects_the_legacy_remote_field() {
        let err = format!(
            "{:#}",
            ClientConfig::parse("remote = \"example.com:2333\"\n").unwrap_err()
        );
        assert!(err.contains("server"), "{}", err);
    }

    #[test]
    fn test_server_setting_does_not_trigger_the_server_config_hint() {
        let err = format!(
            "{:#}",
            ClientConfig::parse("server = \"wss://example.com:2333\"\nunknown = 1\n").unwrap_err()
        );
        assert!(
            !err.contains("no longer takes a server configuration"),
            "{}",
            err
        );
    }

    #[test]
    fn test_example_config() -> Result<()> {
        let paths = get_all_example_config()?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(&p)?;
            // A server, a node and a client example are different schemas.
            let is_server = p.file_name().is_some_and(|name| name == "hts.toml");
            let is_node = p.file_name().is_some_and(|name| name == "htn.toml");
            if is_server {
                let config: ServerConfig = toml::from_str(&s)?;
                assert_eq!(config.server_host, "control.example.com");
                assert!(config.proxies.is_empty(), "{:?}", p);
            } else if is_node {
                let mut config: NodeConfig = toml::from_str(&s)?;
                // The shipped template carries placeholder tokens, so the
                // strict validation must reject it until it is edited.
                assert!(config.validate(&p).is_err(), "{:?} is valid", p);
                config.token = MaskedString::from("downstream-test-token");
                config.upstream.as_mut().unwrap().token = MaskedString::from("upstream-test-token");
                config.validate(&p)?;
                assert_eq!(config.server_host, "b.lan");
                assert!(config.upstream.as_ref().unwrap().mirror);
                assert!(config.upstream.as_ref().unwrap().claim().is_none());
            } else {
                ClientConfig::parse(&s)?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(p)?;
            ClientConfig::parse(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        assert!(!paths.is_empty());
        for p in paths {
            let s = fs::read_to_string(&p)?;
            assert!(ClientConfig::parse(&s).is_err(), "{:?} is valid", p);
        }
        Ok(())
    }

    #[test]
    fn test_resolved_tunnels() -> Result<()> {
        let s = r#"
server = "wss://example.com:2333"
name = "home"
token = "123"
heartbeat_timeout = 20
retry_interval = 7
nodelay = false

[[tunnels]]
name = "nas"
domain = "Foo1.Example.COM"
local_addr = "127.0.0.1:80"

[[tunnels]]
name = "foo2"
domain = "foo2.example.com"
local_addr = "127.0.0.1:81"
"#;
        let cfg = ClientConfig::parse(s)?;

        let tunnels = cfg.resolved_tunnels();
        assert_eq!(tunnels.len(), 2);
        // The domains are lowercased and the order is by name
        assert_eq!(tunnels[0].name, "foo2");
        assert_eq!(tunnels[0].domain, "foo2.example.com");
        assert_eq!(tunnels[0].local_addr, "127.0.0.1:81");
        assert_eq!(tunnels[1].name, "nas");
        assert_eq!(tunnels[1].domain, "foo1.example.com");
        assert_eq!(tunnels[1].local_addr, "127.0.0.1:80");

        // The client-level options are inherited by every tunnel
        assert!(tunnels.iter().all(|t| !t.nodelay));
        assert!(tunnels.iter().all(|t| t.retry_interval == 7));
        assert_eq!(cfg.heartbeat_timeout(), 20);
        Ok(())
    }

    #[test]
    fn test_rejects_the_legacy_tunnels_table() {
        // The old `[tunnels]` table (domain -> local_addr) is no longer accepted
        let s = r#"
name = "home"
token = "123"

[tunnels]
"nas.example.com" = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_set_and_remove_tunnel() -> Result<()> {
        let mut cfg = ClientConfig::parse("name = \"home\"\ntoken = \"123\"\n")?;

        cfg.set_tunnel(
            "nas".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        ClientConfig::validate(&mut cfg)?;
        assert_eq!(cfg.resolved_tunnels().len(), 1);

        // Replacing by name changes the domain without adding a tunnel
        cfg.set_tunnel(
            "nas".to_string(),
            "nas2.example.com".to_string(),
            "127.0.0.1:81".to_string(),
        );
        ClientConfig::validate(&mut cfg)?;
        let tunnels = cfg.resolved_tunnels();
        assert_eq!(tunnels.len(), 1);
        assert_eq!(tunnels[0].domain, "nas2.example.com");
        assert_eq!(tunnels[0].local_addr, "127.0.0.1:81");

        // Removing by name
        assert!(cfg.remove_tunnel("nas"));
        assert!(!cfg.remove_tunnel("nas"));
        assert!(cfg.tunnels.is_empty());
        Ok(())
    }

    #[test]
    fn test_set_tunnel_adds_and_keeps_names_distinct() -> Result<()> {
        let mut cfg = ClientConfig::parse("name = \"home\"\ntoken = \"123\"\n")?;

        cfg.set_tunnel(
            "app-1".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        // A different name serving the same domain is not adopted: it clashes
        cfg.set_tunnel(
            "app-2".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:80".to_string(),
        );
        assert!(ClientConfig::validate(&mut cfg).is_err());

        // Replacing by name is fine
        cfg.set_tunnel(
            "app-1".to_string(),
            "nas.example.com".to_string(),
            "127.0.0.1:81".to_string(),
        );
        ClientConfig::validate(&mut cfg)?;
        let tunnels = cfg.resolved_tunnels();
        assert_eq!(tunnels.len(), 1);
        assert_eq!(tunnels[0].name, "app-1");
        assert_eq!(tunnels[0].local_addr, "127.0.0.1:81");
        Ok(())
    }

    #[test]
    fn test_defaults() -> Result<()> {
        let cfg = ClientConfig::parse("name = \"home\"\ntoken = \"123\"\n")?;
        assert_eq!(cfg.heartbeat_timeout(), DEFAULT_HEARTBEAT_TIMEOUT_SECS);
        assert_eq!(cfg.retry_interval(), DEFAULT_CLIENT_RETRY_INTERVAL_SECS);
        assert!(cfg.nodelay());
        assert!(cfg.tunnels.is_empty());
        Ok(())
    }

    #[test]
    fn test_rejects_an_empty_local_addr() {
        let s = r#"
[[tunnels]]
name = "foo"
domain = "foo.example.com"
local_addr = ""
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_invalid_domain() {
        let s = r#"
[[tunnels]]
name = "foo"
domain = "foo bar"
local_addr = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_domain() {
        let s = r#"
[[tunnels]]
name = "foo1"
domain = "foo.example.com"
local_addr = "127.0.0.1:80"

[[tunnels]]
name = "foo2"
domain = "Foo.Example.com"
local_addr = "127.0.0.1:81"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_unknown_field() {
        let s = r#"
name = "home"
token = "123"
tokenz = "oops"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_name() {
        let s = r#"
[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"

[[tunnels]]
name = "nas"
domain = "git.example.com"
local_addr = "127.0.0.1:81"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_missing_name() {
        let s = r#"
[[tunnels]]
domain = "foo.example.com"
local_addr = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_missing_domain() {
        let s = r#"
[[tunnels]]
name = "nas"
local_addr = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_invalid_name() {
        let s = r#"
[[tunnels]]
name = "nas/../etc"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_parse_rejects_the_legacy_server_config() {
        let legacy = r#"
server_port = 2333
http_port = 80

[clients.home]
token = "123"
"#;
        let err = format!("{:#}", ClientConfig::parse(legacy).unwrap_err());
        assert!(
            err.contains("no longer takes a server configuration"),
            "{}",
            err
        );
    }

    #[test]
    fn test_config_roundtrip() -> Result<()> {
        let s = r#"
server = "wss://example.com:2333"
name = "home"
token = "123"
web_port = 8611
heartbeat_timeout = 20
retry_interval = 2
nodelay = false

[[tunnels]]
name = "nas"
domain = "foo1.example.com"
local_addr = "127.0.0.1:80"

[[tunnels]]
name = "git"
domain = "foo2.example.com"
local_addr = "127.0.0.1:81"
"#;
        let cfg = ClientConfig::parse(s)?;
        let dumped = cfg.to_toml()?;
        let reparsed = ClientConfig::parse(&dumped)?;
        assert_eq!(cfg, reparsed);
        assert!(dumped.contains("[[tunnels]]"), "{}", dumped);
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
name = "home"
token = "123"

[[tunnels]]
name = "foo1"
domain = "foo1.example.com"
local_addr = "127.0.0.1:80"
"#;
        let cfg = ClientConfig::parse(s)?;
        let dumped = cfg.to_toml()?;
        // `None` options must be omitted rather than serialized as an error
        assert!(!dumped.contains("nodelay"));
        assert!(!dumped.contains("server_host"));
        assert!(!dumped.contains("server_port"));
        let reparsed = ClientConfig::parse(&dumped)?;
        assert_eq!(cfg, reparsed);
        Ok(())
    }

    #[test]
    fn test_default_config_is_empty() {
        let cfg = ClientConfig::parse("").unwrap();
        assert_eq!(cfg, ClientConfig::default());
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
    async fn test_server_config_requires_server_host() -> Result<()> {
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-server-host-{}.toml",
            std::process::id()
        ));

        // The WebSocket endpoint is routed by Host, so a server without one
        // would accept no client at all.
        std::fs::write(&path, "server_port = 8080\ntoken = \"a_secret_token\"\n")?;
        let err = ServerConfig::from_file(&path)
            .await
            .expect_err("a missing server_host must be rejected");
        assert!(
            format!("{:#}", err).contains("server_host"),
            "unexpected error: {:#}",
            err
        );

        // The previous names are reported with a rename hint.
        std::fs::write(
            &path,
            "http_port = 8080\n\
             control_host = \"control.example.com\"\ntoken = \"a_secret_token\"\n",
        )?;
        let err = ServerConfig::from_file(&path)
            .await
            .expect_err("the renamed settings must be rejected");
        assert!(
            format!("{:#}", err).contains("renamed to `server_port`"),
            "unexpected error: {:#}",
            err
        );

        std::fs::write(
            &path,
            "server_port = 8080\n\
             server_host = \"control.example.com\"\ntoken = \"a_secret_token\"\n",
        )?;
        let cfg = ServerConfig::from_file(&path).await?;
        assert_eq!(cfg.server_host, "control.example.com");
        assert_eq!(ServerLimits::from(&cfg), ServerLimits::default());

        // The generated template must not start with its placeholder token
        std::fs::write(&path, DEFAULT_SERVER_CONFIG)?;
        let err = ServerConfig::from_file(&path)
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
        let config = ServerConfig::from_file(&path).await?;
        assert_eq!(config.bind_host, "0.0.0.0");

        // A loopback (or IPv6) address is accepted and canonicalized
        std::fs::write(&path, format!("{}bind_host = \"::1\"\n", base))?;
        let config = ServerConfig::from_file(&path).await?;
        assert_eq!(config.bind_host, "::1");

        for invalid in ["localhost", "0.0.0.0:80", "  "] {
            std::fs::write(&path, format!("{}bind_host = {:?}\n", base, invalid))?;
            let err = ServerConfig::from_file(&path)
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
        let default = ClientConfig::parse("server = \"ws://127.0.0.1:8080\"\n")?;
        assert_eq!(default.web_host(), "0.0.0.0");

        let loopback =
            ClientConfig::parse("server = \"ws://127.0.0.1:8080\"\nweb_host = \"127.0.0.1\"\n")?;
        assert_eq!(loopback.web_host(), "127.0.0.1");

        let err =
            ClientConfig::parse("server = \"ws://127.0.0.1:8080\"\nweb_host = \"localhost\"\n")
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
            max_data_connections: 0,
            ..ServerLimits::default()
        };
        assert!(limits.validate().is_err());

        let limits = ServerLimits {
            pool_min: 9,
            pool_max: 8,
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

    /// Check the comment convention of the generated files: `##` marks
    /// documentation and a single `#` only ever disables a setting.
    ///
    /// The distinction matters because a documentation sentence starting with a
    /// single `#` reads like another setting that should be uncommented.
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
    fn test_server_config_example_is_valid() -> Result<()> {
        let example = DEFAULT_SERVER_CONFIG;
        // The template must be a valid server configuration
        let cfg: ServerConfig = toml::from_str(example)?;
        // The token stays a placeholder until the operator picks a strong one
        assert_eq!(
            String::from(&*cfg.token),
            "replace-with-a-long-random-secret"
        );
        assert!(cfg.proxies.is_empty());
        assert_comment_convention(example);
        Ok(())
    }

    #[test]
    fn test_client_config_example_is_valid() -> Result<()> {
        let example = DEFAULT_CLIENT_CONFIG;
        // The commented tunnel is inactive, so the example parses as-is
        let cfg = ClientConfig::parse(example)?;
        assert!(cfg.tunnels.is_empty());
        // The tunnel template is disabled with a single `#`
        assert_eq!(disabled_settings(example, "# [[tunnels]]"), 1);
        assert_comment_convention(example);
        Ok(())
    }

    const NODE_CONFIG: &str = r#"
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

    fn node_config(source: &str) -> NodeConfig {
        toml::from_str(source).expect("the test source should be a valid node TOML")
    }

    fn validate_node(config: &mut NodeConfig) -> Result<()> {
        config.validate(Path::new("htn.toml"))
    }

    #[test]
    fn test_node_config_is_valid() -> Result<()> {
        let mut config = node_config(NODE_CONFIG);
        validate_node(&mut config)?;
        assert_eq!(config.name, "relay-b");
        assert_eq!(config.server_limits(), ServerLimits::default());
        assert_eq!(config.max_mirrored_routes, 256);
        // The upstream endpoint is canonicalized with an explicit port
        assert_eq!(config.upstream.as_ref().unwrap().server, "ws://a.lan:9000");
        assert_eq!(
            config.upstream.as_ref().unwrap().claim(),
            Some("*.b.relay.example.com")
        );
        assert!(!config.upstream.as_ref().unwrap().mirror);
        assert_eq!(
            config.upstream.as_ref().unwrap().server_addr().unwrap(),
            "a.lan:9000"
        );
        assert!(!config.upstream.as_ref().unwrap().tls());

        // An omitted port defaults to the scheme, as for a client
        let mut config = node_config(
            "name = \"relay\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"wss://a.lan\"\n\
             token = \"upstream-secret\"\nclaim = \"svc.b.relay.example.com\"\n",
        );
        validate_node(&mut config)?;
        assert_eq!(config.upstream.as_ref().unwrap().server, "wss://a.lan:443");
        assert!(config.upstream.as_ref().unwrap().tls());
        Ok(())
    }

    #[test]
    fn test_node_without_upstream_is_an_ingress() -> Result<()> {
        let mut config = node_config(
            "name = \"ingress\"\nserver_port = 9001\nserver_host = \"ingress.lan\"\n\
             token = \"downstream-secret\"\n",
        );
        validate_node(&mut config)?;
        assert!(config.upstream.is_none());
        Ok(())
    }

    #[test]
    fn test_node_rejects_an_upstream_pointing_at_itself() {
        // The reserved host on the node's own port is a self-loop
        let mut config = node_config(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://b.lan:9001\"\n\
             token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n",
        );
        assert!(validate_node(&mut config).is_err());

        // A loopback endpoint on the node's own port is a self-loop, even when
        // the host name differs from the reserved one
        for server in [
            "ws://127.0.0.1:9001",
            "ws://localhost:9001",
            "ws://[::1]:9001",
        ] {
            let mut config = node_config(&format!(
                "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
                 token = \"downstream-secret\"\n\n[upstream]\nserver = \"{}\"\n\
                 token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n",
                server
            ));
            assert!(
                validate_node(&mut config).is_err(),
                "{} was accepted",
                server
            );
        }

        // Loopback on another port is a legitimate first hop
        let mut config = node_config(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://127.0.0.1:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"svc.a.lan\"\n",
        );
        assert!(validate_node(&mut config).is_ok());
    }

    #[test]
    fn test_node_bind_host_must_cover_loopback() {
        // The upstream tunnel forwards visitors back to `127.0.0.1`, so a
        // listener on another single interface could not serve them.
        let mut config = node_config(NODE_CONFIG);
        config.bind_host = "192.0.2.10".to_string();
        let err =
            validate_node(&mut config).expect_err("a non-loopback bind_host must be rejected");
        assert!(
            format!("{:#}", err).contains("bind_host"),
            "unexpected error: {:#}",
            err
        );

        for host in ["127.0.0.1", "0.0.0.0", "::1"] {
            let mut config = node_config(NODE_CONFIG);
            config.bind_host = host.to_string();
            assert!(validate_node(&mut config).is_ok(), "{} was rejected", host);
        }
    }

    #[test]
    fn test_node_rejects_an_overlapping_claim() -> Result<()> {
        // The claim must not contain the reserved host of this node
        let mut config = node_config(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://a.lan:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"*.lan\"\n",
        );
        assert!(validate_node(&mut config).is_err());

        // ... nor the server_host of the next hop, which is the URL host
        let mut config = node_config(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\n\n[upstream]\nserver = \"ws://a.lan:9000\"\n\
             token = \"upstream-secret\"\nclaim = \"a.lan\"\n",
        );
        assert!(validate_node(&mut config).is_err());

        // A local proxy inside the claimed subtree is intentional: the node
        // serves that domain itself and relays the rest downstream.
        let mut config = node_config(NODE_CONFIG);
        validate_node(&mut config)?;
        assert_eq!(
            config.upstream.as_ref().unwrap().claim(),
            Some("*.b.relay.example.com")
        );
        assert_eq!(config.proxies[0].domain, "local.b.relay.example.com");
        Ok(())
    }

    #[test]
    fn test_node_rejects_invalid_settings() {
        // A port of 0 cannot be the loopback target of the upstream tunnel
        let mut config = node_config(NODE_CONFIG);
        config.server_port = 0;
        assert!(validate_node(&mut config).is_err());

        // An empty name is not a useful next-hop identifier
        let mut config = node_config(NODE_CONFIG);
        config.name = "  ".to_string();
        assert!(validate_node(&mut config).is_err());

        // Both tokens are required, and the placeholders of the template are
        // not secrets
        let mut config = node_config(NODE_CONFIG);
        config.token = MaskedString::from("");
        assert!(validate_node(&mut config).is_err());
        let mut config = node_config(NODE_CONFIG);
        config.token = MaskedString::from("replace-with-a-long-random-secret");
        assert!(validate_node(&mut config).is_err());
        let mut config = node_config(NODE_CONFIG);
        config.upstream.as_mut().unwrap().token = MaskedString::from("");
        assert!(validate_node(&mut config).is_err());

        // The upstream endpoint must be a WebSocket URL
        let mut config = node_config(NODE_CONFIG);
        config.upstream.as_mut().unwrap().server = "a.lan:9000".to_string();
        assert!(validate_node(&mut config).is_err());

        // The claim is a domain, not a URL or an empty string
        let mut config = node_config(NODE_CONFIG);
        config.upstream.as_mut().unwrap().claim = Some("http://a.lan".to_string());
        assert!(validate_node(&mut config).is_err());

        // Exactly one of `claim` and `mirror` must be set
        let mut config = node_config(NODE_CONFIG);
        config.upstream.as_mut().unwrap().mirror = true;
        assert!(validate_node(&mut config).is_err());
        let mut config = node_config(NODE_CONFIG);
        config.upstream.as_mut().unwrap().claim = None;
        assert!(validate_node(&mut config).is_err());
    }

    #[test]
    fn test_node_mirror_config_is_valid() -> Result<()> {
        let mut config = node_config(
            "name = \"b\"\nserver_port = 9001\nserver_host = \"b.lan\"\n\
             token = \"downstream-secret\"\nmax_mirrored_routes = 0\n\n\
             [upstream]\nserver = \"wss://a.lan\"\n\
             token = \"upstream-secret\"\nmirror = true\n",
        );
        validate_node(&mut config)?;
        let upstream = config.upstream.as_ref().unwrap();
        assert!(upstream.mirror);
        assert!(upstream.claim().is_none());
        assert_eq!(upstream.server, "wss://a.lan:443");
        // 0 disables the mirror cap
        assert_eq!(config.max_mirrored_routes, 0);
        Ok(())
    }

    #[test]
    fn test_node_example_matches_the_documented_placeholders() -> Result<()> {
        let example = DEFAULT_NODE_CONFIG;
        let mut config: NodeConfig = toml::from_str(example)?;
        // The template is not runnable until the operator sets the tokens
        assert!(validate_node(&mut config).is_err());
        config.token = MaskedString::from("downstream-secret");
        config.upstream.as_mut().unwrap().token = MaskedString::from("upstream-secret");
        validate_node(&mut config)?;
        assert_eq!(config.server_port, 9001);
        assert_eq!(config.server_host, "b.lan");
        assert!(config.upstream.as_ref().unwrap().mirror);
        assert!(config.upstream.as_ref().unwrap().claim().is_none());
        // The template follows the `##` documentation convention
        assert_comment_convention(example);
        Ok(())
    }
}

/// Whether a tunnel domain (exact or wildcard) would overlap the reserved
/// `server_host`. A wildcard overlaps every host below its suffix.
pub(crate) fn domain_overlaps_server_host(domain: &str, server_host: &str) -> bool {
    domain == server_host
        || domain.strip_prefix("*.").is_some_and(|suffix| {
            server_host.len() > suffix.len() && server_host.ends_with(&format!(".{}", suffix))
        })
}

/// Server settings loaded exclusively from the fixed hts.toml file.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Server-side HTTP reverse proxy rules.
    #[serde(default)]
    pub proxies: Vec<ServerProxyEntry>,
    /// Public HTTP entrypoint. It also carries the WebSocket tunnel endpoint.
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    /// The IP address the HTTP entrypoint binds. Defaults to `0.0.0.0` (all
    /// interfaces); set it to `127.0.0.1` to serve through a local reverse proxy.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    /// Shared secret required by every client.
    pub token: MaskedString,
    /// The exact Host reserved for the WebSocket control/data connections.
    pub server_host: String,
    /// The number of authenticated WebSocket upgrades that may wait for their
    /// `ClientHello` at the same time.
    #[serde(default = "default_max_pending_handshakes")]
    pub max_pending_handshakes: usize,
    /// The number of control channels accepted at once.
    #[serde(default = "default_max_control_connections")]
    pub max_control_connections: usize,
    /// The number of data channels accepted at once.
    #[serde(default = "default_max_data_connections")]
    pub max_data_connections: usize,
    /// The base number of warm data channels kept per tunnel.
    #[serde(default = "default_pool_min")]
    pub pool_min: usize,
    /// The upper bound of the warm data channels of one tunnel.
    #[serde(default = "default_pool_max")]
    pub pool_max: usize,
    /// WebSocket ping interval in secs. `0` disables the keepalive pings.
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
}

/// The runtime limits derived from [`ServerConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerLimits {
    pub max_pending_handshakes: usize,
    pub max_control_connections: usize,
    pub max_data_connections: usize,
    pub pool_min: usize,
    pub pool_max: usize,
    pub keepalive_interval: u64,
}

impl Default for ServerLimits {
    fn default() -> Self {
        ServerLimits {
            max_pending_handshakes: DEFAULT_MAX_PENDING_HANDSHAKES,
            max_control_connections: DEFAULT_MAX_CONTROL_CONNECTIONS,
            max_data_connections: DEFAULT_MAX_DATA_CONNECTIONS,
            pool_min: DEFAULT_POOL_MIN,
            pool_max: DEFAULT_POOL_MAX,
            keepalive_interval: DEFAULT_KEEPALIVE_INTERVAL_SECS,
        }
    }
}

impl From<&ServerConfig> for ServerLimits {
    fn from(config: &ServerConfig) -> Self {
        ServerLimits {
            max_pending_handshakes: config.max_pending_handshakes,
            max_control_connections: config.max_control_connections,
            max_data_connections: config.max_data_connections,
            pool_min: config.pool_min,
            pool_max: config.pool_max,
            keepalive_interval: config.keepalive_interval,
        }
    }
}

impl ServerLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pending_handshakes == 0
            || self.max_control_connections == 0
            || self.max_data_connections == 0
        {
            bail!("The connection limits must be greater than 0");
        }
        if self.pool_min == 0 {
            bail!("`pool_min` must be greater than 0");
        }
        if self.pool_max < self.pool_min {
            bail!("`pool_max` must not be smaller than `pool_min`");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerProxyEntry {
    pub name: String,
    pub domain: String,
    pub local_addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header: Option<MaskedString>,
}

fn default_server_port() -> u16 {
    80
}
fn default_bind_host() -> String {
    crate::helper::DEFAULT_BIND_HOST.to_string()
}
fn default_max_pending_handshakes() -> usize {
    DEFAULT_MAX_PENDING_HANDSHAKES
}
fn default_max_control_connections() -> usize {
    DEFAULT_MAX_CONTROL_CONNECTIONS
}
fn default_max_data_connections() -> usize {
    DEFAULT_MAX_DATA_CONNECTIONS
}
fn default_pool_min() -> usize {
    DEFAULT_POOL_MIN
}
fn default_pool_max() -> usize {
    DEFAULT_POOL_MAX
}
fn default_keepalive_interval() -> u64 {
    DEFAULT_KEEPALIVE_INTERVAL_SECS
}

impl ServerConfig {
    pub async fn from_file(path: &Path) -> Result<Self> {
        match fs::read_to_string(path).await {
            Ok(s) => {
                let mut config: Self = toml::from_str(&s).with_context(|| {
                    let hint = if has_setting(&s, "http_port") || has_setting(&s, "control_host") {
                        "\nNote: `http_port` and `control_host` were renamed to `server_port` \
                         and `server_host`. `server_port` is now the HTTP entry port, so drop the \
                         old TCP listener port of the same name."
                    } else {
                        ""
                    };
                    format!("The configuration {:?} is invalid{}", path, hint)
                })?;
                normalize_bind_host(&mut config.bind_host, "bind_host")?;
                let limits = ServerLimits::from(&config);
                validate_server_settings(
                    &mut config.server_host,
                    &config.token,
                    &config.proxies,
                    &limits,
                    path,
                )?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::write(path, DEFAULT_SERVER_CONFIG)
                    .await
                    .with_context(|| format!("Failed to generate default config {:?}", path))?;
                bail!(
                    "Generated {:?}. Set `server_host` and a strong `token` (e.g. the output of \
                     `openssl rand -hex 16`), use the same token in htc.toml, then run hts again.",
                    path
                );
            }
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }
}

/// The settings of a node, loaded exclusively from the fixed htn.toml file.
///
/// A node is a server that also dials one upstream hop. It listens for visitors
/// and downstream peers, claims one domain (exact or wildcard) on the upstream,
/// and can serve local `[[proxies]]`. Without `[upstream]` it is an ingress, the
/// same role as `hts`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    /// The name of the node. It's announced as the client name of the upstream
    /// control channel, so an upper hop can name the next hop in its logs.
    pub name: String,
    /// The HTTP entrypoint. It carries visitors, peers and the WebSocket
    /// endpoint. It's required (there is no default port) and must not be 0,
    /// because the upstream tunnel forwards visitors back to it on loopback.
    pub server_port: u16,
    /// The IP address the HTTP entrypoint binds. Defaults to `0.0.0.0`. It must
    /// cover the loopback interface, because the upstream tunnel forwards
    /// visitors back to `127.0.0.1:<server_port>`.
    #[serde(default = "default_bind_host")]
    pub bind_host: String,
    /// The exact Host reserved for the WebSocket control/data connections.
    pub server_host: String,
    /// The shared secret required by the downstream clients and nodes.
    pub token: MaskedString,
    /// Local services served by this node itself. They take precedence over
    /// the tunnels of the downstream clients, as on a server.
    #[serde(default)]
    pub proxies: Vec<ServerProxyEntry>,
    /// The next hop towards the public ingress. Remove the table to run this
    /// node as an ingress.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<UpstreamConfig>,
    /// Application-layer heartbeat timeout of the upstream control channel, in
    /// secs. `0` disables the test.
    #[serde(default = "default_heartbeat_timeout")]
    pub heartbeat_timeout: u64,
    /// The interval between retries to reach the upstream, in secs.
    #[serde(default = "default_client_retry_interval")]
    pub retry_interval: u64,
    /// The maximum number of routes mirrored to the next hop. `0` disables the
    /// cap. Raise it together with the next hop's `max_control_connections`.
    #[serde(default = "default_max_mirrored_routes")]
    pub max_mirrored_routes: usize,
    /// The number of authenticated WebSocket upgrades that may wait for their
    /// `ClientHello` at the same time.
    #[serde(default = "default_max_pending_handshakes")]
    pub max_pending_handshakes: usize,
    /// The number of control channels accepted at once.
    #[serde(default = "default_max_control_connections")]
    pub max_control_connections: usize,
    /// The number of data channels accepted at once.
    #[serde(default = "default_max_data_connections")]
    pub max_data_connections: usize,
    /// The base number of warm data channels kept per tunnel.
    #[serde(default = "default_pool_min")]
    pub pool_min: usize,
    /// The upper bound of the warm data channels of one tunnel.
    #[serde(default = "default_pool_max")]
    pub pool_max: usize,
    /// WebSocket ping interval in secs. `0` disables the keepalive pings of the
    /// embedded server and of the upstream tunnel.
    #[serde(default = "default_keepalive_interval")]
    pub keepalive_interval: u64,
}

/// The next hop of a node: the server or node it dials and claims domains on.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// The WebSocket endpoint of the next hop, e.g. `ws://a.lan:9000` or
    /// `wss://relay.example.com`.
    pub server: String,
    /// The shared token of the next hop. It's recommended to make it different
    /// from the downstream `token`, so that each hop has its own trust boundary.
    pub token: MaskedString,
    /// Claim one exact or wildcard domain on the next hop. This is the Stage 1
    /// relay: the upper hops see only this domain. It must not overlap
    /// `server_host`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<String>,
    /// Mirror the domains of the downstream clients and local `[[proxies]]` to
    /// the next hop, one control channel per domain. It must not be combined
    /// with `claim`.
    #[serde(default)]
    pub mirror: bool,
}

impl UpstreamConfig {
    /// The domain claimed on the next hop when the node is a Stage 1 relay.
    pub fn claim(&self) -> Option<&str> {
        self.claim.as_deref()
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

impl NodeConfig {
    /// Read the fixed node config. If it is missing, write the commented example
    /// and ask the user to edit it before starting.
    pub async fn from_file(path: &Path) -> Result<Self> {
        match fs::read_to_string(path).await {
            Ok(s) => {
                let mut config: Self = toml::from_str(&s)
                    .with_context(|| format!("The configuration {:?} is invalid", path))?;
                config.validate(path)?;
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                fs::write(path, DEFAULT_NODE_CONFIG)
                    .await
                    .with_context(|| format!("Failed to generate default config {:?}", path))?;
                bail!(
                    "Generated {:?}. Set `name`, `server_host`, a strong `token` and the \
                     `[upstream]` settings, then run htn again.",
                    path
                );
            }
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }

    /// The runtime limits of the embedded server.
    pub fn server_limits(&self) -> ServerLimits {
        ServerLimits {
            max_pending_handshakes: self.max_pending_handshakes,
            max_control_connections: self.max_control_connections,
            max_data_connections: self.max_data_connections,
            pool_min: self.pool_min,
            pool_max: self.pool_max,
            keepalive_interval: self.keepalive_interval,
        }
    }

    /// Validate a node configuration and normalize it in place.
    ///
    /// The rules keep a node from becoming an accidental open relay or a
    /// self-loop: the tokens must be set, the local routes must not overlap the
    /// reserved `server_host`, and the upstream must not point back at this
    /// node's own listener. A node with no `[upstream]` is a valid ingress.
    pub fn validate(&mut self, path: &Path) -> Result<()> {
        if self.server_port == 0 {
            bail!(
                "`server_port` must not be 0 in {:?}: the upstream tunnel forwards visitors \
                 back to `127.0.0.1:<server_port>`",
                path
            );
        }
        let bind_host = normalize_bind_host(&mut self.bind_host, "bind_host")?;
        if !(bind_host.is_loopback() || bind_host.is_unspecified()) {
            bail!(
                "`bind_host` in {:?} must be `0.0.0.0` or a loopback address: the upstream \
                 tunnel forwards visitors to `127.0.0.1:<server_port>`",
                path
            );
        }
        let limits = self.server_limits();
        validate_server_settings(
            &mut self.server_host,
            &self.token,
            &self.proxies,
            &limits,
            path,
        )?;
        if &*self.token == "replace-with-the-downstream-secret" {
            bail!("Set a strong, non-placeholder `token` in {:?}", path);
        }

        let name = self.name.trim();
        if name.is_empty() {
            bail!("Set a non-empty node `name` in {:?}", path);
        }
        if name.contains('\r') || name.contains('\n') {
            bail!("The node `name` must not contain line breaks");
        }
        self.name = name.to_string();

        let Some(upstream) = self.upstream.as_mut() else {
            return Ok(());
        };

        let (server, host, port) = canonical_server_url(&upstream.server)?;
        upstream.server = server;
        if upstream.token.is_empty() || &*upstream.token == "replace-with-the-upstream-secret" {
            bail!("Set a strong, non-placeholder `[upstream].token` for the next hop");
        }

        match (upstream.mirror, upstream.claim.take()) {
            (true, Some(_)) => bail!(
                "Set either `[upstream].claim` (one exact or wildcard domain) or \
                 `mirror = true` (one control channel per downstream domain), not both"
            ),
            (false, None) => bail!(
                "Set `[upstream].claim` for one exact or wildcard domain, or `mirror = true` \
                 to mirror the domains of the downstream clients"
            ),
            (true, None) => {}
            (false, Some(claim)) => {
                let claim = crate::helper::normalize_domain(&claim)
                    .ok_or_else(|| anyhow!("The `[upstream].claim` `{}` is invalid", claim))?;
                if domain_overlaps_server_host(&claim, &self.server_host) {
                    bail!(
                        "The `[upstream].claim` `{}` overlaps the reserved server_host `{}`",
                        claim,
                        self.server_host
                    );
                }
                // The host of the upstream URL is the `server_host` of the next
                // hop, because the WebSocket client sends it as the `Host`
                // header. A claim covering it can never be registered there.
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
                // A local proxy under the claim is intentional: this node can
                // serve a few domains of its own subtree and relay the rest
                // downstream.
                upstream.claim = Some(claim);
            }
        }

        // A same-port endpoint with the reserved host or a loopback/unspecified
        // address dials this node's own listener: the visitor would be routed
        // back into the uphill tunnel forever. Names that resolve to loopback
        // are caught again at startup (see `node::ensure_upstream_is_not_self`).
        if port == self.server_port
            && (host.eq_ignore_ascii_case(&self.server_host) || is_local_host(&host))
        {
            bail!(
                "`[upstream].server` points at this node's own listener ({}:{}); a node must \
                 dial its next hop, not itself",
                host,
                port
            );
        }

        Ok(())
    }
}

/// Whether a host is the local machine in the forms that cannot be a remote
/// hop when the port is the node's own: `localhost`, a loopback address or the
/// unspecified address.
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

/// Parse and canonicalize an IP address that a listener binds. A host name is
/// rejected: binding would resolve it at listen time, hiding a typo until the
/// listener fails to start.
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

/// Validate the settings an `hts.toml` and an `htn.toml` share: the reserved
/// host, the limits, the local proxies and the shared token.
fn validate_server_settings(
    server_host: &mut String,
    token: &MaskedString,
    proxies: &[ServerProxyEntry],
    limits: &ServerLimits,
    path: &Path,
) -> Result<()> {
    let normalized = crate::helper::normalize_domain(server_host)
        .ok_or_else(|| anyhow!("Invalid server_host"))?;
    if normalized.starts_with("*.") {
        bail!("server_host must be an exact hostname, not a wildcard");
    }
    *server_host = normalized;
    limits.validate()?;
    let mut domains = BTreeMap::new();
    for proxy in proxies {
        if proxy.name.trim().is_empty() || proxy.local_addr.trim().is_empty() {
            bail!("Each [[proxies]] entry needs non-empty name and local_addr");
        }
        if proxy
            .auth_header
            .as_ref()
            .is_some_and(|v| v.contains('\r') || v.contains('\n'))
        {
            bail!(
                "The `auth_header` of proxy `{}` must not contain line breaks",
                proxy.name
            );
        }
        let domain = crate::helper::normalize_domain(&proxy.domain)
            .ok_or_else(|| anyhow!("Invalid proxy domain `{}`", proxy.domain))?;
        if domain_overlaps_server_host(&domain, server_host) {
            bail!(
                "Proxy domain `{}` overlaps the reserved server_host `{}`",
                domain,
                server_host
            );
        }
        if domains.insert(domain.clone(), ()).is_some() {
            bail!("Proxy domain `{}` is configured more than once", domain);
        }
    }
    if token.is_empty() || &**token == "replace-with-a-long-random-secret" {
        bail!("Set a strong, non-placeholder shared token in {:?}", path);
    }
    Ok(())
}

fn default_heartbeat_timeout() -> u64 {
    DEFAULT_HEARTBEAT_TIMEOUT_SECS
}
fn default_client_retry_interval() -> u64 {
    DEFAULT_CLIENT_RETRY_INTERVAL_SECS
}
fn default_max_mirrored_routes() -> usize {
    DEFAULT_MAX_MIRRORED_ROUTES
}
