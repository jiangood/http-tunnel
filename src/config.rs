use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::Path;
use tokio::fs;
use tracing::info;

/// Application-layer heartbeat interval in secs
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 40;

/// The interval between retries to connect to the server
const DEFAULT_CLIENT_RETRY_INTERVAL_SECS: u64 = 1;

/// A bind address is either a bare port (e.g. `"2333"`), which binds to all interfaces
/// (`0.0.0.0`), or a `host:port`.
fn validate_bind_addr(addr: &str, name: &str) -> Result<()> {
    if addr.is_empty() {
        bail!("`{}` is empty", name);
    }
    if addr.parse::<u16>().is_err() && !addr.contains(':') {
        bail!("`{}` ({}) must be a port or a `host:port`", name, addr);
    }
    Ok(())
}

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

/// A tunnel as seen by a client.
///
/// The server generates it from `[clients.<client>]` and pushes it to the client, so
/// that the client doesn't need any configuration of its own.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct ClientTunnelConfig {
    pub name: String,
    /// The address of the local service on the client side
    pub local_addr: String,
    /// Whether to enable TCP_NODELAY
    pub nodelay: Option<bool>,
    /// The interval between retries to connect to the server
    pub retry_interval: u64,
}

/// The configuration of a client.
///
/// It's generated from `[clients.<name>]` by the server and pushed to the client
/// over a config channel. The client holds no configuration of its own.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct ClientConfig {
    pub tunnels: Vec<ClientTunnelConfig>,
    /// Application-layer heartbeat timeout in secs. 0 disables it
    pub heartbeat_timeout: u64,
    /// The interval between retries to connect to the server
    pub retry_interval: u64,
}

/// A tunnel of `[clients.<client>]`, keyed by its domain.
///
/// A tunnel maps a single domain (the `Host` header) to a single `local_addr` on the
/// client side. The tunnel name is the domain, which is also the identity that the
/// server pushes to the client.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerTunnelConfig {
    /// The name of the tunnel, which is always its `domain`
    pub name: String,
    /// The domain (the `Host` header) that is routed to this tunnel
    pub domain: String,
    /// The address of the local service on the client side
    pub local_addr: String,
    /// Whether to enable TCP_NODELAY. Defaults to `[clients.<client>].nodelay`
    pub nodelay: Option<bool>,
    /// The interval between retries to connect to the server. Defaults to
    /// `[clients.<client>].retry_interval`
    pub retry_interval: Option<u64>,
}

/// The configuration of a client of `[clients.<name>]`.
///
/// The name is the identity of the client, which is given to the client via `--name`.
/// The client-level options apply to all of its tunnels, which are declared in the
/// `[clients.<name>.tunnels]` sub-table, keyed by domain:
///
/// ```toml
/// [clients.home]
/// token = "123"
/// nodelay = true
///
/// [clients.home.tunnels]
/// "nas.example.com" = "127.0.0.1:80"
/// "git.example.com" = "127.0.0.1:3000"
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ServerClientConfig {
    /// The name of the client, filled from the map key
    pub name: String,
    /// The token of the client. It's the only way for the client to authenticate
    pub token: MaskedString,
    /// The interval between two application-layer heartbeats, in secs. 0 disables
    /// sending them
    pub heartbeat_interval: Option<u64>,
    /// Application-layer heartbeat timeout in secs. 0 disables it
    pub heartbeat_timeout: Option<u64>,
    /// The interval between retries to connect to the server
    pub retry_interval: Option<u64>,
    /// Whether to enable TCP_NODELAY for the tunnels of this client. Defaults to true
    pub nodelay: Option<bool>,
    /// The tunnels, indexed by their domain
    pub tunnels: HashMap<String, ServerTunnelConfig>,
}

impl<'de> Deserialize<'de> for ServerClientConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            token: MaskedString,
            #[serde(default)]
            heartbeat_interval: Option<u64>,
            #[serde(default)]
            heartbeat_timeout: Option<u64>,
            #[serde(default)]
            retry_interval: Option<u64>,
            #[serde(default)]
            nodelay: Option<bool>,
            #[serde(default)]
            tunnels: HashMap<String, String>,
        }

        let w = Wire::deserialize(deserializer)?;
        let tunnels = w
            .tunnels
            .into_iter()
            .map(|(domain, local_addr)| {
                let tunnel = ServerTunnelConfig {
                    name: domain.clone(),
                    domain: domain.clone(),
                    local_addr,
                    nodelay: None,
                    retry_interval: None,
                };
                (domain, tunnel)
            })
            .collect();

        Ok(ServerClientConfig {
            name: String::new(),
            token: w.token,
            heartbeat_interval: w.heartbeat_interval,
            heartbeat_timeout: w.heartbeat_timeout,
            retry_interval: w.retry_interval,
            nodelay: w.nodelay,
            tunnels,
        })
    }
}

impl Serialize for ServerClientConfig {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeMap;

        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("token", &self.token)?;
        if let Some(v) = &self.heartbeat_interval {
            map.serialize_entry("heartbeat_interval", v)?;
        }
        if let Some(v) = &self.heartbeat_timeout {
            map.serialize_entry("heartbeat_timeout", v)?;
        }
        if let Some(v) = &self.retry_interval {
            map.serialize_entry("retry_interval", v)?;
        }
        if let Some(v) = &self.nodelay {
            map.serialize_entry("nodelay", v)?;
        }

        // The tunnels form a nested table, `[clients.<name>.tunnels]`, keyed by domain
        if !self.tunnels.is_empty() {
            let tunnels: std::collections::BTreeMap<&str, &str> = self
                .tunnels
                .values()
                .map(|t| (t.domain.as_str(), t.local_addr.as_str()))
                .collect();
            map.serialize_entry("tunnels", &tunnels)?;
        }
        map.end()
    }
}

impl ServerClientConfig {
    /// The interval between two application-layer heartbeats sent to the client, in secs
    pub fn heartbeat_interval(&self) -> u64 {
        self.heartbeat_interval
            .unwrap_or(DEFAULT_HEARTBEAT_INTERVAL_SECS)
    }

    /// Build the configuration that is pushed to the client
    pub fn to_client_config(&self) -> ClientConfig {
        let retry_interval = self
            .retry_interval
            .unwrap_or(DEFAULT_CLIENT_RETRY_INTERVAL_SECS);

        let mut tunnels: Vec<ClientTunnelConfig> = self
            .tunnels
            .values()
            .map(|t| ClientTunnelConfig {
                name: t.name.clone(),
                local_addr: t.local_addr.clone(),
                nodelay: Some(t.nodelay.or(self.nodelay).unwrap_or(true)),
                retry_interval: t.retry_interval.unwrap_or(retry_interval),
            })
            .collect();
        // Could be arbitrary, but keep it stable for the logs
        tunnels.sort_by(|a, b| a.name.cmp(&b.name));

        ClientConfig {
            tunnels,
            heartbeat_timeout: self
                .heartbeat_timeout
                .unwrap_or(DEFAULT_HEARTBEAT_TIMEOUT_SECS),
            retry_interval,
        }
    }
}

/// The configuration of a server. It's the only configuration of `http-tunnel`
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The address that the server listens for clients (config/control/data channels)
    pub bind_addr: String,
    /// The address that the server listens for HTTP visitors, routed by the `Host` header
    pub http_bind_addr: String,
    /// The address that the administration API and the minimal web UI listen at.
    /// The API is only started when both this and `api_token` are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_bind_addr: Option<String>,
    /// The token required by the administration API (`Authorization: Bearer <token>`).
    /// The API is only started when both this and `api_bind_addr` are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_token: Option<MaskedString>,
    /// The clients that are allowed to connect, indexed by the name of the client
    #[serde(default)]
    pub clients: HashMap<String, ServerClientConfig>,
}

impl ServerConfig {
    fn from_str(s: &str) -> Result<ServerConfig> {
        let mut config: ServerConfig = toml::from_str(s)
            .map_err(|e| {
                if s.contains("[server") || s.contains("[client]") || s.contains("default_token") {
                    anyhow!(
                        "{}\nNote: `[server]` is no longer needed, and `[client]`, `[server.services]` and \
                         `default_token` are no longer supported. `http-tunnel` only reads the configuration of \
                         the server: clients and their tunnels are defined in `[clients.<name>]`, and a \
                         client is started with `--remote`, `--name` and `--token`.",
                        e
                    )
                } else {
                    anyhow!("{}", e)
                }
            })
            .with_context(|| "Failed to parse the config")?;

        ServerConfig::validate(&mut config)?;

        Ok(config)
    }

    pub async fn from_file(path: &Path) -> Result<ServerConfig> {
        let s: String = match fs::read_to_string(path).await {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Generate a default config with the three ports, so that the server
                // can start without a config file
                let s = ServerConfig::template();
                fs::write(path, s.as_bytes())
                    .await
                    .with_context(|| format!("Failed to create the config {:?}", path))?;
                info!("{:?} was not found. A default config was generated", path);
                return ServerConfig::from_str(&s);
            }
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to read the config {:?}", path))
            }
        };

        ServerConfig::from_str(&s).with_context(|| {
            "Configuration is invalid. Please refer to the configuration specification."
        })
    }

    /// A minimal template that is written when the config file doesn't exist.
    /// It only sets the three ports, so that a fresh server can start and be
    /// configured through the administration API.
    pub fn template() -> String {
        const TEMPLATE: &str = "\
             # http-tunnel server configuration.\n\
             # The clients and their tunnels are defined in `[clients.<name>]`.\n\
             # See https://github.com/jiangood/http-tunnel#configuration\n\
             \n\
             bind_addr = \"2333\" # The port that the server listens for clients\n\
             http_bind_addr = \"80\" # The HTTP entrypoint, routed by the `Host` header\n\
             api_bind_addr = \"2335\" # The administration API and the web UI, on all interfaces\n\
             api_token = \"change_me\" # Required by the administration API\n\
             \n\
             # [clients.home]\n\
             # token = \"use_a_secret_that_only_you_know\"\n\
             #\n\
             # [clients.home.tunnels]\n\
             # \"my_service.example.com\" = \"127.0.0.1:80\"\n";
        TEMPLATE.to_string()
    }

    /// Validate a configuration and normalize it in place: fill the names from the
    /// map keys, lowercase the domains, and reject the ambiguous definitions.
    ///
    /// A client without any tunnel is allowed, so that a configuration can be built
    /// incrementally through the administration API. The rules that are enforced:
    /// a client must have a token, a domain belongs to a single tunnel, and the tokens
    /// are unique (a duplicated token would let a client impersonate another one).
    pub fn validate(config: &mut ServerConfig) -> Result<()> {
        validate_bind_addr(&config.bind_addr, "bind_addr")?;
        validate_bind_addr(&config.http_bind_addr, "http_bind_addr")?;
        if let Some(addr) = &config.api_bind_addr {
            validate_bind_addr(addr, "api_bind_addr")?;
        }

        // domain -> client name
        let mut seen_domains: HashMap<String, String> = HashMap::new();

        for (client_name, client) in &mut config.clients {
            client.name = client_name.clone();

            // Validate against the global rules first, without mutating the client, so
            // that a rejected configuration is left intact.
            let mut renames: Vec<(String, String)> = Vec::with_capacity(client.tunnels.len());
            for (key, t) in &client.tunnels {
                if t.domain.is_empty() {
                    bail!(
                        "A tunnel of the client `{}` has an empty domain",
                        client_name
                    );
                }

                if t.local_addr.is_empty() {
                    bail!(
                        "The `local_addr` of the tunnel `{}` of the client `{}` is empty",
                        t.domain,
                        client_name
                    );
                }

                let domain = t.domain.to_lowercase();
                if let Some(prev) = seen_domains.insert(domain.clone(), client_name.clone()) {
                    if prev == *client_name {
                        bail!(
                            "The domain `{}` of the client `{}` is defined more than once",
                            domain,
                            client_name
                        );
                    }
                    bail!(
                        "The domain `{}` is used by both the client `{}` and `{}`",
                        domain,
                        prev,
                        client_name
                    );
                }

                renames.push((key.clone(), domain));
            }

            // Normalize: re-key the tunnels by their lowercased domain, and resolve the
            // inherited TCP_NODELAY
            for (key, domain) in renames {
                let mut t = client
                    .tunnels
                    .remove(&key)
                    .expect("the tunnel was just read");
                t.domain = domain.clone();
                t.name = domain.clone();
                t.nodelay = Some(t.nodelay.or(client.nodelay).unwrap_or(true));
                client.tunnels.insert(domain, t);
            }
        }

        // A duplicated token allows a client to impersonate another one
        let mut seen_tokens: HashMap<String, String> = HashMap::new();
        for (client_name, client) in &config.clients {
            if client.token.is_empty() {
                bail!("The token of the client `{}` is empty", client_name);
            }

            if let Some(prev) = seen_tokens.insert(client.token.0.clone(), client_name.clone()) {
                bail!(
                    "The token of the client `{}` is also used by the client `{}`",
                    client_name,
                    prev
                );
            }
        }

        Ok(())
    }

    /// Serialize the configuration back to TOML, so that it can be written back to
    /// the file after a change made through the administration API.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).with_context(|| "Failed to serialize the config")
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
            .filter(|x| x.ends_with(".toml"))
            .collect())
    }

    fn client(name: &str, token: &str, tunnels: Vec<(&str, &str)>) -> ServerClientConfig {
        let mut tunnels_map = HashMap::new();
        for (domain, local_addr) in tunnels {
            tunnels_map.insert(
                domain.to_string(),
                ServerTunnelConfig {
                    name: domain.to_string(),
                    domain: domain.to_string(),
                    local_addr: local_addr.to_string(),
                    ..Default::default()
                },
            );
        }
        ServerClientConfig {
            name: name.to_string(),
            token: token.into(),
            tunnels: tunnels_map,
            ..Default::default()
        }
    }

    #[test]
    fn test_example_config() -> Result<()> {
        let paths = get_all_example_config()?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            ServerConfig::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_valid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/valid_config")?;
        for p in paths {
            let s = fs::read_to_string(p)?;
            ServerConfig::from_str(&s)?;
        }
        Ok(())
    }

    #[test]
    fn test_invalid_config() -> Result<()> {
        let paths = list_config_files("tests/config_test/invalid_config")?;
        for p in paths {
            let s = fs::read_to_string(&p)?;
            assert!(ServerConfig::from_str(&s).is_err(), "{:?} is valid", p);
        }
        Ok(())
    }

    #[test]
    fn test_validate_server_config() -> Result<()> {
        let mut cfg = ServerConfig {
            bind_addr: "2333".into(),
            http_bind_addr: "80".into(),
            clients: HashMap::new(),
            ..Default::default()
        };

        // A config without any client is allowed, so that it can be built incrementally
        assert!(ServerConfig::validate(&mut cfg).is_ok());

        // A client without any tunnel is allowed as well
        cfg.clients
            .insert("home".into(), client("home", "123", vec![]));
        assert!(ServerConfig::validate(&mut cfg).is_ok());

        cfg.clients.insert(
            "home".into(),
            client("home", "123", vec![("foo1.example.com", "127.0.0.1:80")]),
        );
        assert!(ServerConfig::validate(&mut cfg).is_ok());
        assert_eq!(cfg.clients["home"].name, "home");
        assert_eq!(
            cfg.clients["home"].tunnels["foo1.example.com"].name,
            "foo1.example.com"
        );

        // An empty local_addr is rejected
        cfg.clients
            .get_mut("home")
            .unwrap()
            .tunnels
            .get_mut("foo1.example.com")
            .unwrap()
            .local_addr = "".into();
        assert!(ServerConfig::validate(&mut cfg).is_err());
        cfg.clients
            .get_mut("home")
            .unwrap()
            .tunnels
            .get_mut("foo1.example.com")
            .unwrap()
            .local_addr = "127.0.0.1:80".into();

        // An empty domain is rejected
        cfg.clients
            .get_mut("home")
            .unwrap()
            .tunnels
            .get_mut("foo1.example.com")
            .unwrap()
            .domain = "".into();
        assert!(ServerConfig::validate(&mut cfg).is_err());
        cfg.clients
            .get_mut("home")
            .unwrap()
            .tunnels
            .get_mut("foo1.example.com")
            .unwrap()
            .domain = "foo1.example.com".into();

        // The domains are lowercased, and the tunnels are re-keyed
        cfg.clients
            .get_mut("home")
            .unwrap()
            .tunnels
            .get_mut("foo1.example.com")
            .unwrap()
            .domain = "Foo1.Example.COM".into();
        assert!(ServerConfig::validate(&mut cfg).is_ok());
        assert_eq!(
            cfg.clients["home"].tunnels["foo1.example.com"].domain,
            "foo1.example.com".to_string()
        );

        // A duplicate domain is rejected, no matter which client defines it
        cfg.clients.insert(
            "office".into(),
            client("office", "456", vec![("Foo1.example.com", "127.0.0.1:81")]),
        );
        assert!(ServerConfig::validate(&mut cfg).is_err());

        // A duplicate token is rejected
        cfg.clients.get_mut("office").unwrap().tunnels.clear();
        cfg.clients.get_mut("office").unwrap().token = "123".into();
        assert!(ServerConfig::validate(&mut cfg).is_err());

        cfg.clients.get_mut("office").unwrap().token = "456".into();
        assert!(ServerConfig::validate(&mut cfg).is_ok());

        Ok(())
    }

    #[test]
    fn test_to_client_config() -> Result<()> {
        let c = client(
            "home",
            "123",
            vec![
                ("foo1.example.com", "127.0.0.1:80"),
                ("foo2.example.com", "127.0.0.1:81"),
            ],
        );

        let pushed = c.to_client_config();
        assert_eq!(pushed.tunnels.len(), 2);
        assert_eq!(pushed.tunnels[0].name, "foo1.example.com");
        assert_eq!(pushed.tunnels[1].name, "foo2.example.com");
        assert_eq!(pushed.heartbeat_timeout, DEFAULT_HEARTBEAT_TIMEOUT_SECS);
        assert_eq!(pushed.retry_interval, DEFAULT_CLIENT_RETRY_INTERVAL_SECS);
        assert!(pushed.tunnels.iter().all(|t| t.retry_interval == 1));
        assert!(pushed.tunnels.iter().all(|t| t.nodelay == Some(true)));

        // The client-level values are inherited by the tunnels
        let mut c2 = c.clone();
        c2.heartbeat_timeout = Some(0);
        c2.retry_interval = Some(7);
        c2.nodelay = Some(false);

        let pushed = c2.to_client_config();
        assert_eq!(pushed.heartbeat_timeout, 0);
        assert_eq!(pushed.retry_interval, 7);
        assert_eq!(pushed.tunnels[0].retry_interval, 7);
        assert_eq!(pushed.tunnels[0].nodelay, Some(false));
        assert_eq!(pushed.tunnels[1].retry_interval, 7);
        assert_eq!(pushed.tunnels[1].nodelay, Some(false));

        Ok(())
    }

    #[test]
    fn test_parse_rejects_the_legacy_config() {
        let legacy = r#"
[client]
remote_addr = "example.com:2333"

[server]
bind_addr = "0.0.0.0:2333"
http_bind_addr = "0.0.0.0:80"
default_token = "123"

[server.services.foo1]
domain = "foo1.example.com"
"#;
        let err = format!("{:#}", ServerConfig::from_str(legacy).unwrap_err());
        assert!(err.contains("no longer supported"), "{}", err);
    }

    #[test]
    fn test_parse_the_new_config() -> Result<()> {
        let s = r#"
bind_addr = "2333"
http_bind_addr = "80"

[clients.home]
token = "123"
heartbeat_interval = 20
nodelay = false

[clients.home.tunnels]
"foo1.example.com" = "127.0.0.1:80"
"foo2.example.com" = "127.0.0.1:81"
"#;
        let cfg = ServerConfig::from_str(s)?;
        let home = &cfg.clients["home"];
        assert_eq!(home.heartbeat_interval, Some(20));
        assert_eq!(home.nodelay, Some(false));
        assert_eq!(home.tunnels.len(), 2);
        let t1 = &home.tunnels["foo1.example.com"];
        assert_eq!(t1.name, "foo1.example.com");
        assert_eq!(t1.local_addr, "127.0.0.1:80");
        // The client-level nodelay is resolved onto the tunnel
        assert_eq!(t1.nodelay, Some(false));
        assert_eq!(home.tunnels["foo2.example.com"].local_addr, "127.0.0.1:81");
        Ok(())
    }

    #[test]
    fn test_rejects_an_empty_token() -> Result<()> {
        let s = r#"
bind_addr = "2333"
http_bind_addr = "80"

[clients.home]
token = ""

[clients.home.tunnels]
"foo1.example.com" = "127.0.0.1:80"
"#;
        assert!(ServerConfig::from_str(s).is_err());
        Ok(())
    }

    #[test]
    fn test_rejects_an_unknown_client_field() {
        let s = r#"
bind_addr = "2333"
http_bind_addr = "80"

[clients.home]
token = "123"
tokenz = "oops"
"#;
        assert!(ServerConfig::from_str(s).is_err());
    }

    #[test]
    fn test_config_roundtrip() -> Result<()> {
        let s = r#"
bind_addr = "2333"
http_bind_addr = "80"
api_bind_addr = "127.0.0.1:2335"
api_token = "admin_secret"

[clients.home]
token = "123"
heartbeat_interval = 20
heartbeat_timeout = 40
retry_interval = 2
nodelay = false

[clients.home.tunnels]
"foo1.example.com" = "127.0.0.1:80"
"foo2.example.com" = "127.0.0.1:81"
"#;
        let cfg = ServerConfig::from_str(s)?;
        let dumped = cfg.to_toml()?;
        // The dump must be parseable again and preserve the structure
        let reparsed = ServerConfig::from_str(&dumped)?;
        assert_eq!(cfg, reparsed);
        assert!(dumped.contains("api_token = \"admin_secret\""));
        assert!(dumped.contains("[clients.home]"));
        assert!(dumped.contains("[clients.home.tunnels]"));
        assert!(dumped.contains("\"foo1.example.com\" = \"127.0.0.1:80\""));
        Ok(())
    }

    #[test]
    fn test_template_is_valid() -> Result<()> {
        let s = ServerConfig::template();
        let cfg = ServerConfig::from_str(&s)?;
        assert_eq!(cfg.bind_addr, "2333");
        assert_eq!(cfg.http_bind_addr, "80");
        assert_eq!(cfg.api_bind_addr.as_deref(), Some("2335"));
        assert!(cfg.api_token.is_some());
        // The sample client is commented out
        assert!(cfg.clients.is_empty());
        Ok(())
    }

    #[test]
    fn test_config_roundtrip_without_optional_fields() -> Result<()> {
        let s = r#"
bind_addr = "2333"
http_bind_addr = "80"

[clients.home]
token = "123"

[clients.home.tunnels]
"foo1.example.com" = "127.0.0.1:80"
"#;
        let cfg = ServerConfig::from_str(s)?;
        let dumped = cfg.to_toml()?;
        // `None` options must be omitted rather than serialized as an error
        assert!(!dumped.contains("api_bind_addr"));
        assert!(!dumped.contains("nodelay"));
        let reparsed = ServerConfig::from_str(&dumped)?;
        assert_eq!(cfg, reparsed);
        Ok(())
    }

    #[test]
    fn test_to_bind_addr() {
        assert_eq!(crate::helper::to_bind_addr("2333"), "0.0.0.0:2333");
        assert_eq!(
            crate::helper::to_bind_addr("127.0.0.1:2333"),
            "127.0.0.1:2333"
        );
        assert_eq!(crate::helper::to_bind_addr("[::]:2333"), "[::]:2333");
    }

    #[test]
    fn test_rejects_a_bad_bind_addr() {
        let s = r#"
bind_addr = "nope"
http_bind_addr = "80"
"#;
        assert!(ServerConfig::from_str(s).is_err());
    }
}
