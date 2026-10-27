use anyhow::{anyhow, bail, Context, Result};
use rand::RngCore;
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
    /// Whether to enable TCP_NODELAY
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
    /// The host of the server, e.g. `example.com`, `127.0.0.1` or `::1`.
    /// With the WebSocket transport it must be the `control_host` of the server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_host: Option<String>,
    /// The control port of the server. With the TCP transport it is the
    /// `server_port` of the server; with the WebSocket transport it is its
    /// `http_port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    /// Optional transport mode: `tcp` (the default), `ws` (plain WebSocket) or
    /// `wss` (WebSocket over TLS). The legacy value `websocket` means `ws`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
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
    /// Application-layer heartbeat timeout in secs. 0 disables the test
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout: Option<u64>,
    /// The interval between retries to connect to the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_interval: Option<u64>,
    /// Whether to enable TCP_NODELAY for the tunnels of this client
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodelay: Option<bool>,
    /// The tunnels declared by the client
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tunnels: Vec<TunnelEntry>,
}

impl ClientConfig {
    /// The `host:port` address of the server, with an IPv6 host bracketed.
    pub fn server_addr(&self) -> Option<String> {
        Some(format_server_addr(
            self.server_host.as_deref()?,
            self.server_port?,
        ))
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

    /// Whether the WebSocket transport is configured (`ws` or `wss`)
    pub fn is_websocket(&self) -> bool {
        matches!(self.transport.as_deref(), Some("ws") | Some("wss"))
    }

    /// Whether the WebSocket transport uses TLS (`wss`)
    pub fn tls(&self) -> bool {
        self.transport.as_deref() == Some("wss")
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
                        "{}\nNote: `remote` is no longer supported. Set `server_host` and \
                         `server_port` instead.",
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
        match fs::read_to_string(path).await {
            Ok(s) => ClientConfig::parse(&s)
                .with_context(|| format!("The configuration {:?} is invalid", path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let example = client_config_example();
                fs::write(path, example).await
                    .with_context(|| format!("Failed to generate default config {:?}", path))?;
                bail!(
                    "Generated {:?}. Edit it with your server address, shared token and tunnel settings, then restart htc.",
                    path
                );
            }
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }

    /// Validate a configuration and normalize it in place: lowercase the tunnel
    /// domains, validate required names, and reject empty, malformed or
    /// duplicated definitions.
    pub fn validate(config: &mut ClientConfig) -> Result<()> {
        if let Some(transport) = config.transport.as_mut() {
            *transport = transport.trim().to_ascii_lowercase();
            // `websocket` is the name used before `ws`/`wss` existed. It always
            // meant a plain WebSocket, so it is an alias for `ws`.
            if transport == "websocket" {
                *transport = "ws".to_string();
            }
            if transport != "tcp" && transport != "ws" && transport != "wss" {
                bail!("Unsupported transport; expected tcp, ws or wss");
            }
        }
        validate_server_address(config)?;
        let mut names: BTreeMap<String, ()> = BTreeMap::new();
        let mut domains: BTreeMap<String, ()> = BTreeMap::new();
        let mut entries: Vec<TunnelEntry> = Vec::new();

        for entry in std::mem::take(&mut config.tunnels) {
            if entry.local_addr.is_empty() {
                bail!("The `local_addr` of the tunnel `{}` is empty", entry.domain);
            }
            if entry.auth_header.as_ref().is_some_and(|v| v.contains('\r') || v.contains('\n')) {
                bail!("The `auth_header` of the tunnel `{}` must not contain line breaks", entry.name);
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

/// Whether the TOML source still declares the removed top-level `remote` key.
fn has_legacy_remote(s: &str) -> bool {
    s.lines().any(|line| {
        line.trim_start()
            .strip_prefix("remote")
            .is_some_and(|rest| rest.trim_start().starts_with('='))
    })
}

/// Validate and normalize the address of the server.
///
/// The host and the port are optional together: a partial configuration is
/// accepted by the parser, but an address with only one of the two is always a
/// mistake. `resolve_identity` rejects a completely missing address at startup.
fn validate_server_address(config: &mut ClientConfig) -> Result<()> {
    match (config.server_host.as_mut(), config.server_port) {
        (None, None) => Ok(()),
        (Some(_), None) => bail!("The server port is missing. Set `server_port`"),
        (None, Some(_)) => bail!("The server host is missing. Set `server_host`"),
        (Some(host), Some(port)) => {
            if port == 0 {
                bail!("The `server_port` must not be 0");
            }
            *host = normalize_server_host(host)?;
            Ok(())
        }
    }
}

/// Normalize and validate a server host.
///
/// A host is a DNS name, an IPv4 address or an IPv6 address. The port lives in
/// `server_port`, so a `host:port` value is rejected with a hint. IPv6 literals
/// may be written with or without brackets.
fn normalize_server_host(raw: &str) -> Result<String> {
    let host = raw.trim();
    if host.is_empty() {
        bail!("The `server_host` must not be empty");
    }
    if host.contains("://") {
        bail!("The `server_host` must not include a scheme; set `server_port` separately");
    }
    if host.chars().any(|c| c.is_whitespace()) {
        bail!("The `server_host` must not contain whitespace");
    }
    if host.contains('/') || host.contains('\\') || host.contains('?') || host.contains('#') {
        bail!("The `server_host` `{}` is not a valid host", raw);
    }

    if let Some(inner) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        inner
            .parse::<std::net::Ipv6Addr>()
            .map_err(|_| anyhow!("The `server_host` `{}` is not a valid IPv6 address", raw))?;
        return Ok(inner.to_string());
    }
    if host.contains('[') || host.contains(']') {
        bail!("The `server_host` `{}` is not a valid host", raw);
    }
    if host.contains(':') {
        host.parse::<std::net::Ipv6Addr>().map_err(|_| {
            anyhow!(
                "The `server_host` `{}` must not include a port; set `server_port` instead",
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

/// Generate a strong random shared token: 128 bits, lowercase hex encoded
fn generate_token() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

/// The commented example written when htc.toml is missing.
///
/// It shows two tunnels: a plain TCP one and a Docker socket one. The token is
/// left as a placeholder because it must match the one of the server.
fn client_config_example() -> String {
    r#"## http-tunnel client configuration
##
## Comment convention:
##   ##  documentation, leave it as it is
##   #   a disabled setting, remove the single # to enable it
##
## Set these values before starting htc.
##
## The TCP transport is the default. `server_port` is the `server_port` of
## hts.toml.
transport = "tcp"
server_host = "your-server.example.com"
server_port = 2333
##
## To use the WebSocket transport, comment out the three settings above and
## uncomment the three below. `server_host` must be the `control_host` of
## hts.toml and `server_port` must be its `http_port`, not its `server_port`.
## `transport = "ws"` is plain `ws://`; `"wss"` is TLS (`wss://`) on any port.
# transport = "wss"
# server_host = "control.example.com"
# server_port = 443
name = "home"
token = "replace-with-the-same-secret-as-hts"
web_port = 8610
heartbeat_timeout = 40
retry_interval = 1
nodelay = true

## Add one [[tunnels]] block per service to expose.
##
## Example 1, a plain TCP service.
## Uncomment the block below to enable it.
# [[tunnels]]
# name = "nas"
# domain = "nas.example.com"
# local_addr = "127.0.0.1:80"

## Example 2, a Docker API reachable over a Unix socket.
## Uncomment the block below to enable it.
## auth_header is optional; when set, requests need X-HTTP-TUNNEL-TOKEN: <secret> or get 401.
# [[tunnels]]
# name = "docker-api"
# domain = "docker.example.com"
# local_addr = "unix:///var/run/docker.sock"
# auth_header = "replace-with-a-long-secret"
"#
    .to_string()
}

/// The commented example written when hts.toml is missing.
///
/// The shared token is generated when the file is first created.
fn server_config_example(token: &str) -> String {
    format!(
        r#"## http-tunnel server configuration
##
## Comment convention:
##   ##  documentation, leave it as it is
##   #   a disabled setting, remove the single # to enable it
##
## The shared token is generated for you. Set the same token in htc.toml.
## Client TCP port. Remove this line to disable the TCP listener and serve
## clients only through the WebSocket transport (requires control_host below).
server_port = 2333
http_port = 80
token = "{token}"
## Optional: reserve this exact Host for WebSocket transport. It must resolve
## to this server.
# control_host = "control.example.com"

## Optional direct reverse-proxy routes to services running on this server.
##
## Example 1, a plain TCP service.
## Uncomment the block below to enable it.
# [[proxies]]
# name = "grafana"
# domain = "grafana.example.com"
# local_addr = "127.0.0.1:3000"

## Example 2, a Docker API reachable over a Unix socket. The path is resolved
## on this host, so inside a container the socket must be mounted, e.g.
## -v /var/run/docker.sock:/var/run/docker.sock
## Uncomment the block below to enable it.
## auth_header is optional; when set, requests need X-HTTP-TUNNEL-TOKEN: <secret> or get 401.
# [[proxies]]
# name = "docker-api"
# domain = "docker.example.com"
# local_addr = "unix:///var/run/docker.sock"
# auth_header = "replace-with-a-long-secret"
"#
    )
}

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
    fn test_transport_validation() -> Result<()> {
        // `websocket` is the legacy spelling of `ws`
        let config = ClientConfig::parse("transport = \"WebSocket\"\n")?;
        assert_eq!(config.transport.as_deref(), Some("ws"));
        assert!(config.is_websocket());
        assert!(!config.tls());

        assert!(ClientConfig::parse("transport = \"udp\"\n").is_err());
        assert!(ClientConfig::parse("transport = \"tls\"\n").is_err());

        let tcp = ClientConfig::parse("")?;
        assert!(!tcp.is_websocket());
        assert!(!tcp.tls());
        Ok(())
    }

    #[test]
    fn test_ws_and_wss_transports() -> Result<()> {
        let ws = ClientConfig::parse("transport = \"WS\"\n")?;
        assert_eq!(ws.transport.as_deref(), Some("ws"));
        assert!(ws.is_websocket());
        assert!(!ws.tls());

        let wss = ClientConfig::parse("transport = \"wss\"\n")?;
        assert_eq!(wss.transport.as_deref(), Some("wss"));
        assert!(wss.is_websocket());
        assert!(wss.tls());
        Ok(())
    }

    #[test]
    fn test_server_addr() -> Result<()> {
        let cfg = ClientConfig::parse("server_host = \"example.com\"\nserver_port = 2333\n")?;
        assert_eq!(cfg.server_addr().as_deref(), Some("example.com:2333"));

        let cfg = ClientConfig::parse("server_host = \"::1\"\nserver_port = 2333\n")?;
        assert_eq!(cfg.server_host.as_deref(), Some("::1"));
        assert_eq!(cfg.server_addr().as_deref(), Some("[::1]:2333"));

        // Brackets are accepted and normalized away
        let cfg = ClientConfig::parse("server_host = \"[::1]\"\nserver_port = 2333\n")?;
        assert_eq!(cfg.server_host.as_deref(), Some("::1"));
        Ok(())
    }

    #[test]
    fn test_rejects_a_partial_server_address() {
        assert!(ClientConfig::parse("server_host = \"example.com\"\n").is_err());
        assert!(ClientConfig::parse("server_port = 2333\n").is_err());
        let err = format!(
            "{:#}",
            ClientConfig::parse("server_host = \"example.com\"\n").unwrap_err()
        );
        assert!(err.contains("server_port"), "{}", err);
    }

    #[test]
    fn test_rejects_an_invalid_server_address() {
        for source in [
            "server_host = \"example.com:2333\"\nserver_port = 2333\n",
            "server_host = \"tcp://example.com\"\nserver_port = 2333\n",
            "server_host = \"\"\nserver_port = 2333\n",
            "server_host = \"example.com\"\nserver_port = 0\n",
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
        assert!(err.contains("server_host"), "{}", err);
        assert!(err.contains("server_port"), "{}", err);
    }

    #[test]
    fn test_server_port_does_not_trigger_the_server_config_hint() {
        let err = format!(
            "{:#}",
            ClientConfig::parse("server_host = \"example.com\"\nserver_port = 2333\nunknown = 1\n")
                .unwrap_err()
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
            let s = fs::read_to_string(p)?;
            ClientConfig::parse(&s)?;
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
server_host = "example.com"
server_port = 2333
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
        assert!(tunnels.iter().all(|t| t.nodelay == false));
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
        assert!(err.contains("no longer takes a server configuration"), "{}", err);
    }

    #[test]
    fn test_config_roundtrip() -> Result<()> {
        let s = r#"
server_host = "example.com"
server_port = 2333
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
        assert!(dumped.contains("domain = \"foo1.example.com\""), "{}", dumped);
        assert!(dumped.contains("local_addr = \"127.0.0.1:80\""), "{}", dumped);
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
    fn test_generate_token_is_strong_and_random() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 32);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert_ne!(a, "replace-with-a-long-random-secret");
    }

    #[test]
    fn test_domain_cannot_overlap_control_host() {
        assert!(domain_overlaps_control_host("control.example.com", "control.example.com"));
        assert!(domain_overlaps_control_host("*.example.com", "control.example.com"));
        assert!(!domain_overlaps_control_host("*.example.com", "example.com"));
        assert!(!domain_overlaps_control_host("*.other.test", "control.example.com"));
    }

    #[tokio::test]
    async fn test_server_config_allows_a_disabled_tcp_listener() -> Result<()> {
        // Omitting `server_port` disables the TCP listener. Without a control
        // host the clients would have no way to connect, so that is rejected.
        let path = std::env::temp_dir().join(format!(
            "http-tunnel-zero-port-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "http_port = 8080\ntoken = \"a_secret_token\"\n")?;
        let err = ServerConfig::from_file(&path)
            .await
            .expect_err("a missing server_port without control_host must be rejected");
        assert!(
            format!("{:#}", err).contains("control_host"),
            "unexpected error: {:#}",
            err
        );

        // `0` is not a magic value; removing the line is how the listener is
        // disabled, so an explicit zero is a configuration error.
        std::fs::write(
            &path,
            "server_port = 0\nhttp_port = 8080\n\
             control_host = \"control.example.com\"\ntoken = \"a_secret_token\"\n",
        )?;
        let err = ServerConfig::from_file(&path)
            .await
            .expect_err("server_port = 0 must be rejected");
        assert!(
            format!("{:#}", err).contains("real port"),
            "unexpected error: {:#}",
            err
        );

        // With a control host and no `server_port`, the TCP listener is off.
        std::fs::write(
            &path,
            "http_port = 8080\n\
             control_host = \"control.example.com\"\ntoken = \"a_secret_token\"\n",
        )?;
        let cfg = ServerConfig::from_file(&path).await?;
        let _ = std::fs::remove_file(&path);
        assert_eq!(cfg.server_port, None);
        assert_eq!(cfg.control_host.as_deref(), Some("control.example.com"));
        Ok(())
    }

    /// The number of lines that disable a setting with a single `#`.
    fn disabled_settings(example: &str, prefix: &str) -> usize {
        example.lines().filter(|line| line.starts_with(prefix)).count()
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
        let token = generate_token();
        let example = server_config_example(&token);
        // The generated example must be a valid server configuration
        let cfg: ServerConfig = toml::from_str(&example)?;
        assert_eq!(String::from(&*cfg.token), token);
        assert!(cfg.proxies.is_empty());
        // A plain and a Docker socket proxy example are both disabled
        assert_eq!(disabled_settings(&example, "# [[proxies]]"), 2);
        assert!(example.contains("unix:///var/run/docker.sock"));
        assert_comment_convention(&example);
        // The header documentation stays on one line: a wrapped comment reads
        // like a second setting and misleads whoever edits the file.
        assert_eq!(
            example.lines().filter(|l| l.contains("X-HTTP-TUNNEL-TOKEN")).count(),
            1
        );
        // Only the Docker example documents the optional `auth_header`
        let (plain, docker) = example
            .split_once("## Example 2")
            .expect("the Docker proxy example is present");
        assert!(!plain.contains("auth_header"), "{}", plain);
        assert!(docker.contains("auth_header"), "{}", docker);
        Ok(())
    }

    #[test]
    fn test_client_config_example_is_valid() -> Result<()> {
        let example = client_config_example();
        // The commented tunnels are inactive, so the example parses as-is
        let cfg = ClientConfig::parse(&example)?;
        assert!(cfg.tunnels.is_empty());
        // A plain and a Docker socket tunnel example are both disabled
        assert_eq!(disabled_settings(&example, "# [[tunnels]]"), 2);
        assert!(example.contains("unix:///var/run/docker.sock"));
        assert_comment_convention(&example);
        // The header documentation stays on one line: a wrapped comment reads
        // like a second setting and misleads whoever edits the file.
        assert_eq!(
            example.lines().filter(|l| l.contains("X-HTTP-TUNNEL-TOKEN")).count(),
            1
        );
        // Only the Docker tunnel documents the optional `auth_header`
        let (plain, docker) = example
            .split_once("## Example 2")
            .expect("the Docker tunnel example is present");
        assert!(!plain.contains("auth_header"), "{}", plain);
        assert!(docker.contains("auth_header"), "{}", docker);
        Ok(())
    }
}


/// Whether a tunnel domain (exact or wildcard) would overlap the reserved
/// `control_host`. A wildcard overlaps every host below its suffix.
pub(crate) fn domain_overlaps_control_host(domain: &str, control_host: &str) -> bool {
    domain == control_host
        || domain.strip_prefix("*.").is_some_and(|suffix| {
            control_host.len() > suffix.len()
                && control_host.ends_with(&format!(".{}", suffix))
        })
}

/// Server settings loaded exclusively from the fixed hts.toml file.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Server-side HTTP reverse proxy rules.
    #[serde(default)]
    pub proxies: Vec<ServerProxyEntry>,
    /// Port for client control and data connections. Omit it to disable the
    /// TCP listener, leaving the WebSocket transport as the only client
    /// entrypoint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_port: Option<u16>,
    /// Public HTTP entrypoint.
    #[serde(default = "default_http_port")]
    pub http_port: u16,
    /// Shared secret required by every client.
    pub token: MaskedString,
    /// Optional exact Host reserved for WebSocket control/data connections.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_host: Option<String>,
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

fn default_http_port() -> u16 { 80 }

impl ServerConfig {
    pub async fn from_file(path: &Path) -> Result<Self> {
        match fs::read_to_string(path).await {
            Ok(s) => {
                let mut config: Self = toml::from_str(&s)
                    .with_context(|| format!("The configuration {:?} is invalid", path))?;
                if let Some(host) = config.control_host.as_deref() {
                    let normalized = crate::helper::normalize_domain(host)
                        .ok_or_else(|| anyhow!("Invalid control_host"))?;
                    if normalized.starts_with("*.") {
                        bail!("control_host must be an exact hostname, not a wildcard");
                    }
                    config.control_host = Some(normalized);
                }
                let mut domains = BTreeMap::new();
                for proxy in &config.proxies {
                    if proxy.name.trim().is_empty() || proxy.local_addr.trim().is_empty() {
                        bail!("Each [[proxies]] entry needs non-empty name and local_addr");
                    }
                    if proxy.auth_header.as_ref().is_some_and(|v| v.contains('\r') || v.contains('\n')) {
                        bail!("The `auth_header` of proxy `{}` must not contain line breaks", proxy.name);
                    }
                    let domain = crate::helper::normalize_domain(&proxy.domain)
                        .ok_or_else(|| anyhow!("Invalid proxy domain `{}`", proxy.domain))?;
                    if let Some(control_host) = config.control_host.as_deref() {
                        let conflicts = domain_overlaps_control_host(&domain, control_host);
                        if conflicts {
                            bail!(
                                "Proxy domain `{}` overlaps the reserved control_host `{}`",
                                domain,
                                control_host
                            );
                        }
                    }
                    if domains.insert(domain.clone(), ()).is_some() {
                        bail!("Proxy domain `{}` is configured more than once", domain);
                    }
                }
                match config.server_port {
                    Some(0) => bail!(
                        "`server_port` must be a real port; remove the line to disable the TCP \
                         listener and serve clients only through the WebSocket transport"
                    ),
                    None if config.control_host.is_none() => bail!(
                        "`server_port` is not set, so the TCP listener is disabled, but \
                         `control_host` is not set either: clients would have no way to connect"
                    ),
                    _ => {}
                }
                if config.token.is_empty()
                    || &*config.token == "replace-with-a-long-random-secret"
                {
                    bail!("Set a strong, non-placeholder shared token in {:?}", path);
                }
                Ok(config)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let token = generate_token();
                let example = server_config_example(&token);
                fs::write(path, example).await
                    .with_context(|| format!("Failed to generate default config {:?}", path))?;
                bail!(
                    "Generated {:?} with the shared token {:?}. Set the same token in htc.toml, then run hts again.",
                    path, token
                );
            }
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }
}
