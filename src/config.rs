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
    /// The address of the server, e.g. `example.com:2333`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// Optional transport mode. Missing means the legacy TCP transport.
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
                if s.contains("server_port") || s.contains("http_port") || s.contains("[clients") {
                    anyhow!(
                        "{}\nNote: `http-tunnel` no longer takes a server configuration file. The \
                         configure the server in `hts.toml`; client routes belong in \
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
            if transport.as_str() != "tcp" && transport.as_str() != "websocket" {
                bail!("Unsupported transport; expected tcp or websocket");
            }
        }
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
    r#"# http-tunnel client configuration
# Set these values before starting htc.
remote = "your-server.example.com:2333"
# Optional: "tcp" (default) or "websocket". For WebSocket, set remote to "control.example.com:80"
# and configure the same control_host in hts.toml.
# transport = "websocket"
name = "home"
token = "replace-with-the-same-secret-as-hts"
web_port = 8610
heartbeat_timeout = 40
retry_interval = 1
nodelay = true

# Add one [[tunnels]] block per service to expose.
# A plain TCP service:
# [[tunnels]]
# name = "nas"
# domain = "nas.example.com"
# local_addr = "127.0.0.1:80"

# A service reachable over a Unix socket:
# [[tunnels]]
# name = "docker-api"
# domain = "docker.example.com"
# local_addr = "unix:///var/run/docker.sock"
# auth_header is optional; when set, a request must carry
# `X-HTTP-TUNNEL-TOKEN: <secret>` or it is answered with 401
# auth_header = "replace-with-a-long-secret"
"#
    .to_string()
}

/// The commented example written when hts.toml is missing.
///
/// The shared token is generated when the file is first created.
fn server_config_example(token: &str) -> String {
    format!(
        r#"# http-tunnel server configuration
# The shared token is generated for you. Set the same token in htc.toml.
server_port = 2333
http_port = 80
token = "{token}"
# Optional: reserve this exact Host for WebSocket transport. It must resolve to this server.
# control_host = "control.example.com"

# Optional direct reverse-proxy routes to services running on this server.
# A plain TCP service:
# [[proxies]]
# name = "grafana"
# domain = "grafana.example.com"
# local_addr = "127.0.0.1:3000"

# A service reachable over a Unix socket:
# [[proxies]]
# name = "docker-api"
# domain = "docker.example.com"
# local_addr = "unix:///var/run/docker.sock"
# auth_header is optional; when set, a request must carry
# `X-HTTP-TUNNEL-TOKEN: <secret>` or it is answered with 401
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
remote = "example.com:2333"
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
remote = "example.com:2333"
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
        assert!(!dumped.contains("remote"));
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
    fn test_server_config_example_is_valid() -> Result<()> {
        let token = generate_token();
        let example = server_config_example(&token);
        // The generated example must be a valid server configuration
        let cfg: ServerConfig = toml::from_str(&example)?;
        assert_eq!(String::from(&*cfg.token), token);
        assert!(cfg.proxies.is_empty());
        // A plain and a Docker socket proxy example are both present
        assert_eq!(example.matches("# [[proxies]]").count(), 2);
        assert!(example.contains("unix:///var/run/docker.sock"));
        Ok(())
    }

    #[test]
    fn test_client_config_example_is_valid() -> Result<()> {
        let example = client_config_example();
        // The commented tunnels are inactive, so the example parses as-is
        let cfg = ClientConfig::parse(&example)?;
        assert!(cfg.tunnels.is_empty());
        // A plain and a Docker socket tunnel example are both present
        assert_eq!(example.matches("# [[tunnels]]").count(), 2);
        assert!(example.contains("unix:///var/run/docker.sock"));
        // Only the Docker tunnel documents the optional `auth_header`
        let (plain, docker) = example
            .split_once("# A service reachable over a Unix socket:")
            .expect("the Docker tunnel example is present");
        assert!(!plain.contains("auth_header"), "{}", plain);
        assert!(docker.contains("auth_header"), "{}", docker);
        Ok(())
    }
}


/// Server settings loaded exclusively from the fixed hts.toml file.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Server-side HTTP reverse proxy rules.
    #[serde(default)]
    pub proxies: Vec<ServerProxyEntry>,
    /// Port for client control and data connections.
    #[serde(default = "default_server_port")]
    pub server_port: u16,
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

fn default_server_port() -> u16 { 2333 }
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
                    if domains.insert(domain.clone(), ()).is_some() {
                        bail!("Proxy domain `{}` is configured more than once", domain);
                    }
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
