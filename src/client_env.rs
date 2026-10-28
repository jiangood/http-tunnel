//! Environment-variable configuration of the client.
//!
//! Every setting of `htc.toml` has an `HTC_*` counterpart. The environment wins
//! over the file field by field, and `HTC_TUNNELS` replaces the tunnels of the
//! file as a whole, so a container can be configured without a mounted file.
//!
//! A variable that is unset or blank is ignored. That lets a compose file pass
//! `HTC_TOKEN=` when the value is not known yet, without overriding the file.

use crate::config::{ClientConfig, MaskedString, TunnelEntry};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// The WebSocket endpoint of the server, e.g. `wss://control.example.com:443`.
pub(crate) const ENV_SERVER: &str = "HTC_SERVER";
/// The name of the client.
pub(crate) const ENV_NAME: &str = "HTC_NAME";
/// The shared token of the client.
pub(crate) const ENV_TOKEN: &str = "HTC_TOKEN";
/// The port of the client web management interface.
pub(crate) const ENV_WEB_PORT: &str = "HTC_WEB_PORT";
/// The address the client web management interface binds.
pub(crate) const ENV_WEB_HOST: &str = "HTC_WEB_HOST";
/// The application-layer heartbeat timeout in secs.
pub(crate) const ENV_HEARTBEAT_TIMEOUT: &str = "HTC_HEARTBEAT_TIMEOUT";
/// The WebSocket ping interval in secs.
pub(crate) const ENV_KEEPALIVE_INTERVAL: &str = "HTC_KEEPALIVE_INTERVAL";
/// The interval between retries to connect to the server.
pub(crate) const ENV_RETRY_INTERVAL: &str = "HTC_RETRY_INTERVAL";
/// The tunnels of the client, as a TOML array of inline tables.
pub(crate) const ENV_TUNNELS: &str = "HTC_TUNNELS";

/// The client settings that the environment overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ClientEnv {
    server: Option<String>,
    name: Option<String>,
    token: Option<MaskedString>,
    web_port: Option<u16>,
    web_host: Option<String>,
    heartbeat_timeout: Option<u64>,
    keepalive_interval: Option<u64>,
    retry_interval: Option<u64>,
    tunnels: Option<Vec<TunnelEntry>>,
    /// The names of the variables that were set, in documentation order
    vars: Vec<&'static str>,
}

impl ClientEnv {
    /// Read the environment of the process.
    pub(crate) fn from_process() -> Result<ClientEnv> {
        ClientEnv::from_lookup(|key| match std::env::var_os(key) {
            None => Ok(None),
            Some(value) => value
                .into_string()
                .map(Some)
                .map_err(|_| anyhow!("The environment variable {} is not valid Unicode", key)),
        })
    }

    /// Read the settings from `lookup`, which maps a variable name to its value.
    ///
    /// `lookup` returns an error for a value that is not valid Unicode, so that
    /// a broken secret is reported instead of being silently ignored.
    pub(crate) fn from_lookup<F>(lookup: F) -> Result<ClientEnv>
    where
        F: Fn(&str) -> Result<Option<String>>,
    {
        let mut env = ClientEnv::default();

        if let Some(value) = read(&lookup, ENV_SERVER)? {
            env.vars.push(ENV_SERVER);
            env.server = Some(value);
        }
        if let Some(value) = read(&lookup, ENV_NAME)? {
            env.vars.push(ENV_NAME);
            env.name = Some(value);
        }
        if let Some(value) = read(&lookup, ENV_TOKEN)? {
            env.vars.push(ENV_TOKEN);
            env.token = Some(MaskedString::from(value));
        }
        if let Some(value) = read(&lookup, ENV_WEB_PORT)? {
            env.vars.push(ENV_WEB_PORT);
            env.web_port = Some(parse_number(ENV_WEB_PORT, &value, "a port number")?);
        }
        if let Some(value) = read(&lookup, ENV_WEB_HOST)? {
            env.vars.push(ENV_WEB_HOST);
            env.web_host = Some(parse_ip(ENV_WEB_HOST, &value)?);
        }
        if let Some(value) = read(&lookup, ENV_HEARTBEAT_TIMEOUT)? {
            env.vars.push(ENV_HEARTBEAT_TIMEOUT);
            env.heartbeat_timeout = Some(parse_number(
                ENV_HEARTBEAT_TIMEOUT,
                &value,
                "a number of seconds",
            )?);
        }
        if let Some(value) = read(&lookup, ENV_KEEPALIVE_INTERVAL)? {
            env.vars.push(ENV_KEEPALIVE_INTERVAL);
            env.keepalive_interval = Some(parse_number(
                ENV_KEEPALIVE_INTERVAL,
                &value,
                "a number of seconds",
            )?);
        }
        if let Some(value) = read(&lookup, ENV_RETRY_INTERVAL)? {
            env.vars.push(ENV_RETRY_INTERVAL);
            env.retry_interval = Some(parse_number(
                ENV_RETRY_INTERVAL,
                &value,
                "a number of seconds",
            )?);
        }
        if let Some(value) = read(&lookup, ENV_TUNNELS)? {
            env.vars.push(ENV_TUNNELS);
            env.tunnels = Some(parse_tunnels(&value)?);
        }

        Ok(env)
    }

    /// Overlay the environment on a configuration.
    pub(crate) fn apply(&self, config: &mut ClientConfig) {
        if let Some(server) = &self.server {
            config.server = Some(server.clone());
        }
        if let Some(name) = &self.name {
            config.name = Some(name.clone());
        }
        if let Some(token) = &self.token {
            config.token = Some(token.clone());
        }
        if let Some(web_port) = self.web_port {
            config.web_port = Some(web_port);
        }
        if let Some(web_host) = &self.web_host {
            config.web_host = Some(web_host.clone());
        }
        if let Some(heartbeat_timeout) = self.heartbeat_timeout {
            config.heartbeat_timeout = Some(heartbeat_timeout);
        }
        if let Some(keepalive_interval) = self.keepalive_interval {
            config.keepalive_interval = Some(keepalive_interval);
        }
        if let Some(retry_interval) = self.retry_interval {
            config.retry_interval = Some(retry_interval);
        }
        // The tunnels replace the ones of the file as a whole. Merging would
        // make a removed tunnel linger until the process restarts.
        if let Some(tunnels) = &self.tunnels {
            config.tunnels = tunnels.clone();
        }
    }

    /// Whether the tunnels come from `HTC_TUNNELS` rather than from the file.
    pub(crate) fn tunnels_managed(&self) -> bool {
        self.tunnels.is_some()
    }

    /// The names of the variables that were set, without their values.
    pub(crate) fn vars(&self) -> &[&'static str] {
        &self.vars
    }

    /// Whether the environment overrides no setting at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.vars.is_empty()
    }
}

/// Read a variable, treating an unset or blank value as absent.
fn read<F>(lookup: &F, key: &str) -> Result<Option<String>>
where
    F: Fn(&str) -> Result<Option<String>>,
{
    Ok(lookup(key)?.filter(|value| !value.trim().is_empty()))
}

/// Parse a numeric variable, naming it in the error.
fn parse_number<T>(key: &str, raw: &str, expected: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    raw.trim()
        .parse::<T>()
        .map_err(|_| anyhow!("{} must be {}, got {:?}", key, expected, raw))
}

/// Parse an IP address variable and store it in canonical form.
fn parse_ip(key: &str, raw: &str) -> Result<String> {
    raw.trim()
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| anyhow!("{} must be an IP address, got {:?}", key, raw))
}

/// The `HTC_TUNNELS` value, wrapped so that the existing TOML parser reads it.
#[derive(Deserialize)]
struct TunnelsDocument {
    tunnels: Vec<TunnelEntry>,
}

/// Parse `HTC_TUNNELS`, e.g.
/// `[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]`.
fn parse_tunnels(raw: &str) -> Result<Vec<TunnelEntry>> {
    let source = format!("tunnels = {}", raw.trim());
    let document: TunnelsDocument = toml::from_str(&source)
        .with_context(|| format!("{} is not a valid TOML array of tunnels", ENV_TUNNELS))?;
    Ok(document.tunnels)
}

/// The client configuration after merging the file and the environment.
#[derive(Debug)]
pub(crate) struct LoadedConfig {
    /// The configuration the client runs with, environment included.
    pub(crate) effective: ClientConfig,
    /// The configuration as read from the file, without the environment. The
    /// administration API writes its changes back to this one, so that an
    /// environment value is never persisted by accident.
    pub(crate) persisted: ClientConfig,
    /// The environment settings that override the file.
    pub(crate) env: ClientEnv,
}

/// Load `htc.toml` and overlay the `HTC_*` environment variables.
///
/// A missing file without any `HTC_*` keeps the historical bootstrap: the
/// commented example is written and an error asks the user to edit it. A
/// missing file with `HTC_*` starts from the defaults instead, so a container
/// needs no mounted configuration.
pub(crate) async fn load(path: &Path) -> Result<LoadedConfig> {
    load_with(path, ClientEnv::from_process()?).await
}

async fn load_with(path: &Path, env: ClientEnv) -> Result<LoadedConfig> {
    let persisted = match ClientConfig::read(path).await? {
        Some(source) => ClientConfig::parse(&source)
            .with_context(|| format!("The configuration {:?} is invalid", path))?,
        None if env.is_empty() => return Err(ClientConfig::bootstrap_example(path).await),
        None => ClientConfig::default(),
    };

    let mut effective = persisted.clone();
    env.apply(&mut effective);
    ClientConfig::validate(&mut effective)
        .with_context(|| "The configuration after applying the HTC_* environment is invalid")?;

    Ok(LoadedConfig {
        effective,
        persisted,
        env,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;

    /// Build an environment from pairs, without touching the process one.
    fn env(pairs: &[(&str, &str)]) -> Result<ClientEnv> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        ClientEnv::from_lookup(|key| Ok(map.get(key).cloned()))
    }

    /// A unique path in the temporary directory, removed by the caller.
    fn temp_path(tag: &str) -> PathBuf {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let count = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "http-tunnel-env-{}-{}-{}.toml",
            std::process::id(),
            tag,
            count
        ))
    }

    const ENV_TUNNEL: &str = r#"[{name="nas",domain="NAS.Example.com",local_addr="127.0.0.1:80"},
        {name="docker",domain="docker.example.com",local_addr="unix:///var/run/docker.sock",auth_header="secret"}]"#;

    #[test]
    fn test_reads_every_setting() -> Result<()> {
        let parsed = env(&[
            (ENV_SERVER, "wss://control.example.com"),
            (ENV_NAME, "home"),
            (ENV_TOKEN, "secret"),
            (ENV_WEB_PORT, "0"),
            (ENV_WEB_HOST, "127.0.0.1"),
            (ENV_HEARTBEAT_TIMEOUT, "20"),
            (ENV_KEEPALIVE_INTERVAL, "5"),
            (ENV_RETRY_INTERVAL, "3"),
            (ENV_TUNNELS, ENV_TUNNEL),
        ])?;

        let mut config = ClientConfig::default();
        parsed.apply(&mut config);
        ClientConfig::validate(&mut config)?;

        // The canonical form is produced by the same validation as the file
        assert_eq!(
            config.server.as_deref(),
            Some("wss://control.example.com:443")
        );
        assert_eq!(config.name.as_deref(), Some("home"));
        assert_eq!(&**config.token.as_ref().unwrap(), "secret");
        assert_eq!(config.web_port, Some(0));
        assert_eq!(config.web_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(config.heartbeat_timeout, Some(20));
        assert_eq!(config.keepalive_interval, Some(5));
        assert_eq!(config.retry_interval, Some(3));

        let tunnels = config.resolved_tunnels();
        assert_eq!(tunnels.len(), 2);
        assert_eq!(tunnels[0].name, "docker");
        assert_eq!(tunnels[0].local_addr, "unix:///var/run/docker.sock");
        assert_eq!(&**tunnels[0].auth_header.as_ref().unwrap(), "secret");
        assert_eq!(tunnels[1].domain, "nas.example.com");

        assert_eq!(
            parsed.vars().to_vec(),
            vec![
                ENV_SERVER,
                ENV_NAME,
                ENV_TOKEN,
                ENV_WEB_PORT,
                ENV_WEB_HOST,
                ENV_HEARTBEAT_TIMEOUT,
                ENV_KEEPALIVE_INTERVAL,
                ENV_RETRY_INTERVAL,
                ENV_TUNNELS,
            ]
        );
        assert!(parsed.tunnels_managed());
        Ok(())
    }

    #[test]
    fn test_blank_values_are_ignored() -> Result<()> {
        let parsed = env(&[(ENV_NAME, "  "), (ENV_TOKEN, ""), (ENV_TUNNELS, "\n")])?;
        assert!(parsed.is_empty());
        assert!(!parsed.tunnels_managed());
        assert_eq!(parsed, ClientEnv::default());
        Ok(())
    }

    #[test]
    fn test_invalid_values_name_the_variable() {
        let err = format!("{:#}", env(&[(ENV_WEB_PORT, "abc")]).unwrap_err());
        assert!(err.contains(ENV_WEB_PORT), "{}", err);

        let err = format!("{:#}", env(&[(ENV_WEB_HOST, "localhost")]).unwrap_err());
        assert!(err.contains(ENV_WEB_HOST), "{}", err);

        let err = format!("{:#}", env(&[(ENV_HEARTBEAT_TIMEOUT, "-1")]).unwrap_err());
        assert!(err.contains(ENV_HEARTBEAT_TIMEOUT), "{}", err);

        let err = format!("{:#}", env(&[(ENV_TUNNELS, "{oops")]).unwrap_err());
        assert!(err.contains(ENV_TUNNELS), "{}", err);

        // An unknown tunnel field must not be silently discarded
        let err = format!(
            "{:#}",
            env(&[(
                ENV_TUNNELS,
                r#"[{name="x",domain="x.example.com",local_addr="1.2.3.4:5",oops=1}]"#
            )])
            .unwrap_err()
        );
        assert!(err.contains(ENV_TUNNELS), "{}", err);
    }

    #[test]
    fn test_scalar_override_keeps_the_file_tunnels() -> Result<()> {
        let mut config = ClientConfig::parse(
            r#"
server = "wss://file.example.com:443"
token = "file-token"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let parsed = env(&[(ENV_TOKEN, "env-token"), (ENV_NAME, "container")])?;
        parsed.apply(&mut config);
        ClientConfig::validate(&mut config)?;

        assert_eq!(&**config.token.as_ref().unwrap(), "env-token");
        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].name, "nas");
        assert!(!parsed.tunnels_managed());
        Ok(())
    }

    #[test]
    fn test_tunnels_replace_the_file_ones() -> Result<()> {
        let mut config = ClientConfig::parse(
            r#"
[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let parsed = env(&[(
            ENV_TUNNELS,
            r#"[{name="git",domain="git.example.com",local_addr="127.0.0.1:81"}]"#,
        )])?;
        parsed.apply(&mut config);
        ClientConfig::validate(&mut config)?;

        assert_eq!(config.tunnels.len(), 1);
        assert_eq!(config.tunnels[0].name, "git");
        Ok(())
    }

    #[test]
    fn test_an_empty_array_clears_the_tunnels() -> Result<()> {
        let parsed = env(&[(ENV_TUNNELS, "[]")])?;
        assert!(parsed.tunnels_managed());

        let mut config = ClientConfig::parse(
            r#"
[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        parsed.apply(&mut config);
        ClientConfig::validate(&mut config)?;
        assert!(config.tunnels.is_empty());
        Ok(())
    }

    #[test]
    fn test_duplicate_names_and_domains_are_rejected() -> Result<()> {
        for tunnels in [
            r#"[{name="nas",domain="a.example.com",local_addr="127.0.0.1:80"},
                {name="nas",domain="b.example.com",local_addr="127.0.0.1:81"}]"#,
            r#"[{name="a",domain="same.example.com",local_addr="127.0.0.1:80"},
                {name="b",domain="SAME.example.com",local_addr="127.0.0.1:81"}]"#,
        ] {
            let parsed = env(&[(ENV_TUNNELS, tunnels)])?;
            let mut config = ClientConfig::default();
            parsed.apply(&mut config);
            let err = format!("{:#}", ClientConfig::validate(&mut config).unwrap_err());
            assert!(err.contains("more than once"), "{}", err);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_load_without_a_file_uses_the_environment() -> Result<()> {
        let path = temp_path("env-only");
        let env = env(&[
            (ENV_SERVER, "ws://127.0.0.1:8080"),
            (ENV_NAME, "home"),
            (ENV_TOKEN, "secret"),
            (ENV_TUNNELS, ENV_TUNNEL),
        ])?;

        let loaded = load_with(&path, env).await?;
        assert!(!path.exists(), "the example file must not be generated");
        assert_eq!(loaded.persisted, ClientConfig::default());
        assert_eq!(loaded.effective.tunnels.len(), 2);
        assert!(loaded.env.tunnels_managed());
        Ok(())
    }

    #[tokio::test]
    async fn test_load_without_a_file_and_without_the_environment_writes_the_example() -> Result<()>
    {
        let path = temp_path("bootstrap");
        let err = load_with(&path, ClientEnv::default())
            .await
            .expect_err("the first start must ask the user to edit the example");
        assert!(format!("{:#}", err).contains("Generated"), "{}", err);
        let example = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        assert!(example.contains("[[tunnels]]"), "{}", example);
        Ok(())
    }

    #[tokio::test]
    async fn test_load_prefers_the_environment_and_keeps_the_file_baseline() -> Result<()> {
        let path = temp_path("baseline");
        std::fs::write(
            &path,
            r#"
server = "wss://file.example.com:443"
name = "file"
token = "file-token"

[[tunnels]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let env = env(&[(ENV_TOKEN, "env-token"), (ENV_TUNNELS, ENV_TUNNEL)])?;
        let loaded = load_with(&path, env).await?;
        let _ = std::fs::remove_file(&path);

        // Runtime uses the environment
        assert_eq!(&**loaded.effective.token.as_ref().unwrap(), "env-token");
        assert_eq!(loaded.effective.tunnels.len(), 2);

        // The baseline is what the API may write back
        assert_eq!(&**loaded.persisted.token.as_ref().unwrap(), "file-token");
        assert_eq!(loaded.persisted.tunnels.len(), 1);
        assert_eq!(loaded.persisted.tunnels[0].name, "nas");
        Ok(())
    }
}
