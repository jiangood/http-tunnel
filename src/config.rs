use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::{Debug, Formatter};
use std::ops::Deref;
use std::path::Path;
use tokio::fs;

/// Application-layer heartbeat interval in secs
const DEFAULT_HEARTBEAT_INTERVAL_SECS: u64 = 30;
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

/// A service as seen by a client.
///
/// The server generates it from `[clients.<client>.services.<service>]` and pushes
/// it to the client, so that the client doesn't need any configuration of its own.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct ClientServiceConfig {
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
    pub services: Vec<ClientServiceConfig>,
    /// Application-layer heartbeat timeout in secs. 0 disables it
    pub heartbeat_timeout: u64,
    /// The interval between retries to connect to the server
    pub retry_interval: u64,
}

/// A service of `[clients.<client>.services.<service>]`
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerServiceConfig {
    #[serde(skip)]
    pub name: String,
    /// The hosts (the `Host` header) that are routed to this service
    #[serde(default)]
    pub hosts: Vec<String>,
    /// The address of the service on the client side
    pub local_addr: String,
    /// Whether to enable TCP_NODELAY. Defaults to `[clients.<client>].nodelay`,
    /// then to `true`
    pub nodelay: Option<bool>,
    /// The interval between retries to connect to the server. Defaults to
    /// `[clients.<client>].retry_interval`
    pub retry_interval: Option<u64>,
}

/// A client of `[clients.<name>]`.
///
/// The name is the identity of the client, which is given to the client via `--name`.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerClientConfig {
    #[serde(skip)]
    pub name: String,
    /// The token of the client. It's the only way for the client to authenticate
    pub token: MaskedString,
    /// Application-layer heartbeat timeout in secs. 0 disables it
    pub heartbeat_timeout: Option<u64>,
    /// The interval between retries to connect to the server
    pub retry_interval: Option<u64>,
    /// Whether to enable TCP_NODELAY for the services of this client
    pub nodelay: Option<bool>,
    pub services: HashMap<String, ServerServiceConfig>,
}

impl ServerClientConfig {
    /// Build the configuration that is pushed to the client
    pub fn to_client_config(&self) -> ClientConfig {
        let retry_interval = self
            .retry_interval
            .unwrap_or(DEFAULT_CLIENT_RETRY_INTERVAL_SECS);

        let mut services: Vec<ClientServiceConfig> = self
            .services
            .values()
            .map(|s| ClientServiceConfig {
                name: s.name.clone(),
                local_addr: s.local_addr.clone(),
                nodelay: s.nodelay.or(self.nodelay),
                retry_interval: s.retry_interval.unwrap_or(retry_interval),
            })
            .collect();
        // Could be arbitrary, but keep it stable for the logs
        services.sort_by(|a, b| a.name.cmp(&b.name));

        ClientConfig {
            services,
            heartbeat_timeout: self
                .heartbeat_timeout
                .unwrap_or(DEFAULT_HEARTBEAT_TIMEOUT_SECS),
            retry_interval,
        }
    }
}

fn default_heartbeat_interval() -> u64 {
    DEFAULT_HEARTBEAT_INTERVAL_SECS
}

/// The configuration of a server. It's the only configuration of `rathole`
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The address that the server listens for clients (config/control/data channels)
    pub bind_addr: String,
    /// The address that the server listens for HTTP visitors, routed by the `Host` header
    pub http_bind_addr: String,
    #[serde(default = "default_heartbeat_interval")]
    pub heartbeat_interval: u64,
    /// The clients that are allowed to connect, indexed by the name of the client
    pub clients: HashMap<String, ServerClientConfig>,
}

impl ServerConfig {
    fn from_str(s: &str) -> Result<ServerConfig> {
        let mut config: ServerConfig = toml::from_str(s)
            .map_err(|e| {
                if s.contains("[server") || s.contains("[client") || s.contains("default_token") {
                    anyhow!(
                        "{}\nNote: `[server]` is no longer needed, and `[client]`, `[server.services]` and \
                         `default_token` are no longer supported. `rathole` only reads the configuration of \
                         the server: clients and their services are defined in `[clients.<name>]`, and a \
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
        let s: String = fs::read_to_string(path)
            .await
            .with_context(|| format!("Failed to read the config {:?}", path))?;
        ServerConfig::from_str(&s).with_context(|| {
            "Configuration is invalid. Please refer to the configuration specification."
        })
    }

    fn validate(config: &mut ServerConfig) -> Result<()> {
        if config.clients.is_empty() {
            bail!("No client is defined in `[clients]`");
        }

        // host -> service name and service name -> client name
        let mut seen_hosts: HashMap<String, String> = HashMap::new();
        let mut seen_services: HashMap<String, String> = HashMap::new();

        for (client_name, client) in &mut config.clients {
            client.name = client_name.clone();

            if client.services.is_empty() {
                bail!("No service is defined for the client `{}`", client_name);
            }

            for (service_name, s) in &mut client.services {
                s.name = service_name.clone();

                if s.local_addr.is_empty() {
                    bail!(
                        "The `local_addr` of the service `{}` of the client `{}` is empty",
                        service_name,
                        client_name
                    );
                }

                if s.hosts.is_empty() {
                    bail!(
                        "The `hosts` of the service `{}` of the client `{}` is empty",
                        service_name,
                        client_name
                    );
                }

                if let Some(prev) = seen_services.insert(service_name.clone(), client_name.clone())
                {
                    bail!(
                        "The service `{}` is defined by both the client `{}` and `{}`",
                        service_name,
                        prev,
                        client_name
                    );
                }

                for h in &mut s.hosts {
                    *h = h.to_lowercase();
                    if let Some(prev) = seen_hosts.insert(h.clone(), service_name.clone()) {
                        bail!(
                            "The host `{}` is used by both the service `{}` and `{}`",
                            h,
                            prev,
                            service_name
                        );
                    }
                }
            }
        }

        // A duplicated token allows a client to impersonate another one
        let mut seen_tokens: HashMap<String, String> = HashMap::new();
        for (client_name, client) in &config.clients {
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

    fn client(
        name: &str,
        token: &str,
        services: Vec<(&str, &str, Vec<&str>)>,
    ) -> ServerClientConfig {
        let mut services_map = HashMap::new();
        for (sname, local_addr, hosts) in services {
            services_map.insert(
                sname.to_string(),
                ServerServiceConfig {
                    name: sname.to_string(),
                    hosts: hosts.into_iter().map(|h| h.to_string()).collect(),
                    local_addr: local_addr.to_string(),
                    ..Default::default()
                },
            );
        }
        ServerClientConfig {
            name: name.to_string(),
            token: token.into(),
            services: services_map,
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
            clients: HashMap::new(),
            ..Default::default()
        };

        // No client is rejected
        assert!(ServerConfig::validate(&mut cfg).is_err());

        cfg.clients
            .insert("home".into(), client("home", "123", vec![]));

        // No service is rejected
        assert!(ServerConfig::validate(&mut cfg).is_err());

        cfg.clients.insert(
            "home".into(),
            client(
                "home",
                "123",
                vec![("foo1", "127.0.0.1:80", vec!["foo1.example.com"])],
            ),
        );
        assert!(ServerConfig::validate(&mut cfg).is_ok());
        assert_eq!(cfg.clients["home"].name, "home");
        assert_eq!(cfg.clients["home"].services["foo1"].name, "foo1");

        // An empty local_addr is rejected
        cfg.clients
            .get_mut("home")
            .unwrap()
            .services
            .get_mut("foo1")
            .unwrap()
            .local_addr = "".into();
        assert!(ServerConfig::validate(&mut cfg).is_err());
        cfg.clients
            .get_mut("home")
            .unwrap()
            .services
            .get_mut("foo1")
            .unwrap()
            .local_addr = "127.0.0.1:80".into();

        // Empty hosts is rejected
        cfg.clients
            .get_mut("home")
            .unwrap()
            .services
            .get_mut("foo1")
            .unwrap()
            .hosts = vec![];
        assert!(ServerConfig::validate(&mut cfg).is_err());
        cfg.clients
            .get_mut("home")
            .unwrap()
            .services
            .get_mut("foo1")
            .unwrap()
            .hosts = vec!["foo1.example.com".into()];

        // The hosts are lowercased
        cfg.clients
            .get_mut("home")
            .unwrap()
            .services
            .get_mut("foo1")
            .unwrap()
            .hosts = vec!["Foo1.Example.COM".into()];
        assert!(ServerConfig::validate(&mut cfg).is_ok());
        assert_eq!(
            cfg.clients["home"].services["foo1"].hosts,
            vec!["foo1.example.com".to_string()]
        );

        // A duplicate host is rejected, no matter which client defines it
        cfg.clients.insert(
            "office".into(),
            client(
                "office",
                "456",
                vec![("foo2", "127.0.0.1:81", vec!["foo1.example.com"])],
            ),
        );
        assert!(ServerConfig::validate(&mut cfg).is_err());

        // A duplicate service name is rejected
        cfg.clients
            .get_mut("office")
            .unwrap()
            .services
            .get_mut("foo2")
            .unwrap()
            .hosts = vec!["foo2.example.com".into()];
        cfg.clients.get_mut("office").unwrap().services.insert(
            "foo1".into(),
            ServerServiceConfig {
                hosts: vec!["foo2.example.com".into()],
                local_addr: "127.0.0.1:81".into(),
                ..Default::default()
            },
        );
        cfg.clients
            .get_mut("office")
            .unwrap()
            .services
            .remove("foo2");
        assert!(ServerConfig::validate(&mut cfg).is_err());

        // A duplicate token is rejected
        cfg.clients
            .get_mut("office")
            .unwrap()
            .services
            .remove("foo1");
        cfg.clients.get_mut("office").unwrap().services.insert(
            "foo3".into(),
            ServerServiceConfig {
                hosts: vec!["foo3.example.com".into()],
                local_addr: "127.0.0.1:83".into(),
                ..Default::default()
            },
        );
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
                ("foo1", "127.0.0.1:80", vec!["foo1.example.com"]),
                ("foo2", "127.0.0.1:81", vec!["foo2.example.com"]),
            ],
        );

        let pushed = c.to_client_config();
        assert_eq!(pushed.services.len(), 2);
        assert_eq!(pushed.services[0].name, "foo1");
        assert_eq!(pushed.services[1].name, "foo2");
        assert_eq!(pushed.heartbeat_timeout, DEFAULT_HEARTBEAT_TIMEOUT_SECS);
        assert_eq!(pushed.retry_interval, DEFAULT_CLIENT_RETRY_INTERVAL_SECS);
        assert!(pushed.services.iter().all(|s| s.retry_interval == 1));
        assert!(pushed.services.iter().all(|s| s.nodelay.is_none()));

        // The client-level values are inherited by the services
        let mut c2 = c.clone();
        c2.heartbeat_timeout = Some(0);
        c2.retry_interval = Some(7);
        c2.nodelay = Some(false);
        c2.services.get_mut("foo2").unwrap().retry_interval = Some(9);

        let pushed = c2.to_client_config();
        assert_eq!(pushed.heartbeat_timeout, 0);
        assert_eq!(pushed.retry_interval, 7);
        assert_eq!(pushed.services[0].retry_interval, 7);
        assert_eq!(pushed.services[0].nodelay, Some(false));
        assert_eq!(pushed.services[1].retry_interval, 9);

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
hosts = ["foo1.example.com"]
"#;
        let err = format!("{:#}", ServerConfig::from_str(legacy).unwrap_err());
        assert!(err.contains("no longer supported"), "{}", err);
    }

    #[test]
    fn test_parse_the_new_config() -> Result<()> {
        let s = r#"
bind_addr = "0.0.0.0:2333"
http_bind_addr = "0.0.0.0:80"

[clients.home]
token = "123"

[clients.home.services.foo1]
hosts = ["foo1.example.com"]
local_addr = "127.0.0.1:80"
"#;
        let cfg = ServerConfig::from_str(s)?;
        assert_eq!(cfg.clients["home"].services["foo1"].name, "foo1");
        Ok(())
    }
}
