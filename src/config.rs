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
/// The client derives it from the `[tunnels]` table of its configuration file.
/// The name is the domain (the `Host` header) that the tunnel serves, which is
/// also the identity the client announces on the control channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTunnelConfig {
    /// The domain (the `Host` header) that is routed to this tunnel
    pub name: String,
    /// The address of the local service on the client side
    pub local_addr: String,
    /// Whether to enable TCP_NODELAY
    pub nodelay: bool,
    /// The interval between retries to connect to the server
    pub retry_interval: u64,
}

/// The configuration of a client.
///
/// It's the source of truth of `http-tunnel`: the server takes no configuration
/// file and learns the tunnels of a client from the control channels it opens.
/// The identity (`remote`, `name` and `token`) can also be given on the command
/// line or through the environment, in which case the command line wins.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ClientConfig {
    /// The address of the server, e.g. `example.com:2333`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    /// The name of the client. It's used for logging and to warn when a domain
    /// changes hands
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The token of the client. It must match the shared token of the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<MaskedString>,
    /// The port of the client administration API. Defaults to 8610, `0` disables it
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_port: Option<u16>,
    /// Application-layer heartbeat timeout in secs. 0 disables the test
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_timeout: Option<u64>,
    /// The interval between retries to connect to the server
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_interval: Option<u64>,
    /// Whether to enable TCP_NODELAY for the tunnels of this client
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodelay: Option<bool>,
    /// The tunnels, each a domain keyed to the `local_addr` of the local service
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tunnels: BTreeMap<String, String>,
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

    /// The resolved tunnels, sorted by domain for a stable order
    pub fn resolved_tunnels(&self) -> Vec<ClientTunnelConfig> {
        let retry_interval = self.retry_interval();
        let nodelay = self.nodelay();
        let mut tunnels: Vec<ClientTunnelConfig> = self
            .tunnels
            .iter()
            .map(|(domain, local_addr)| ClientTunnelConfig {
                name: domain.clone(),
                local_addr: local_addr.clone(),
                nodelay,
                retry_interval,
            })
            .collect();
        tunnels.sort_by(|a, b| a.name.cmp(&b.name));
        tunnels
    }

    /// Parse a configuration and normalize it
    pub fn parse(s: &str) -> Result<ClientConfig> {
        let mut config: ClientConfig = toml::from_str(s)
            .map_err(|e| {
                if s.contains("server_port") || s.contains("http_port") || s.contains("[clients") {
                    anyhow!(
                        "{}\nNote: `http-tunnel` no longer takes a server configuration file. The \
                         whole configuration now lives on the client: the server is started with \
                         `--server-port`, `--http-port` and `--token`, and the tunnels are declared \
                         in the `[tunnels]` table of `client.toml`.",
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

    /// Read the configuration file. A missing file is not an error: it yields an
    /// empty configuration, so that a client can also be started with the
    /// command line only (`--remote`, `--name` and `--token`).
    pub async fn from_file(path: &Path) -> Result<ClientConfig> {
        match fs::read_to_string(path).await {
            Ok(s) => ClientConfig::parse(&s)
                .with_context(|| format!("The configuration {:?} is invalid", path)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ClientConfig::default()),
            Err(e) => Err(e).with_context(|| format!("Failed to read the config {:?}", path)),
        }
    }

    /// Validate a configuration and normalize it in place: lowercase the tunnel
    /// domains and reject the empty or duplicated definitions.
    pub fn validate(config: &mut ClientConfig) -> Result<()> {
        let mut tunnels: BTreeMap<String, String> = BTreeMap::new();
        for (domain, local_addr) in std::mem::take(&mut config.tunnels) {
            if local_addr.is_empty() {
                bail!("The `local_addr` of the tunnel `{}` is empty", domain);
            }

            let domain = crate::helper::normalize_domain(&domain)
                .ok_or_else(|| anyhow!("The domain `{}` is invalid", domain))?;

            if tunnels.insert(domain.clone(), local_addr).is_some() {
                bail!("The domain `{}` is defined more than once", domain);
            }
        }
        config.tunnels = tunnels;

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

[tunnels]
"Foo1.Example.COM" = "127.0.0.1:80"
"foo2.example.com" = "127.0.0.1:81"
"#;
        let mut cfg = ClientConfig::parse(s)?;
        ClientConfig::validate(&mut cfg)?;

        let tunnels = cfg.resolved_tunnels();
        assert_eq!(tunnels.len(), 2);
        // The domains are lowercased and sorted
        assert_eq!(tunnels[0].name, "foo1.example.com");
        assert_eq!(tunnels[0].local_addr, "127.0.0.1:80");
        assert_eq!(tunnels[1].name, "foo2.example.com");

        // The client-level options are inherited by every tunnel
        assert!(tunnels.iter().all(|t| t.nodelay == false));
        assert!(tunnels.iter().all(|t| t.retry_interval == 7));
        assert_eq!(cfg.heartbeat_timeout(), 20);
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
[tunnels]
"foo.example.com" = ""
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_an_invalid_domain() {
        let s = r#"
[tunnels]
"foo bar" = "127.0.0.1:80"
"#;
        assert!(ClientConfig::parse(s).is_err());
    }

    #[test]
    fn test_rejects_a_duplicated_domain() {
        let s = r#"
[tunnels]
"foo.example.com" = "127.0.0.1:80"
"Foo.Example.com" = "127.0.0.1:81"
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
api_port = 8611
heartbeat_timeout = 20
retry_interval = 2
nodelay = false

[tunnels]
"foo1.example.com" = "127.0.0.1:80"
"foo2.example.com" = "127.0.0.1:81"
"#;
        let cfg = ClientConfig::parse(s)?;
        let dumped = cfg.to_toml()?;
        let reparsed = ClientConfig::parse(&dumped)?;
        assert_eq!(cfg, reparsed);
        assert!(dumped.contains("[tunnels]"));
        assert!(dumped.contains("\"foo1.example.com\" = \"127.0.0.1:80\""));
        Ok(())
    }

    #[test]
    fn test_config_roundtrip_without_optional_fields() -> Result<()> {
        let s = r#"
name = "home"
token = "123"

[tunnels]
"foo1.example.com" = "127.0.0.1:80"
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
}
