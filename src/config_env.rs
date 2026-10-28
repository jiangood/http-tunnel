//! Environment-variable configuration of the universal process.
//!
//! The common settings of `ht.toml` have an `HT_*` counterpart. The environment
//! wins over the file field by field, and `HT_PROXIES` replaces the local
//! services of the file as a whole, so a container can be configured without a
//! mounted file.
//!
//! A variable that is unset or blank is ignored. That lets a compose file pass
//! `HT_TOKEN=` when the value is not known yet, without overriding the file.

use crate::config::{Config, ConfigSource, MaskedString, ServerProxyEntry, UpstreamConfig};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// The WebSocket endpoint of the next hop, e.g. `wss://control.example.com:443`.
pub(crate) const ENV_UPSTREAM_URL: &str = "HT_UPSTREAM_URL";
/// The name of the process.
pub(crate) const ENV_NAME: &str = "HT_NAME";
/// The listener token shared with downstream peers.
pub(crate) const ENV_TOKEN: &str = "HT_TOKEN";
/// The listener port.
pub(crate) const ENV_LISTEN_PORT: &str = "HT_LISTEN_PORT";
/// The Host reserved for the v4 session endpoint.
pub(crate) const ENV_CONTROL_HOST: &str = "HT_CONTROL_HOST";
/// The WebSocket ping interval in secs.
pub(crate) const ENV_KEEPALIVE_INTERVAL: &str = "HT_KEEPALIVE_INTERVAL";
/// The interval between retries to connect to the next hop.
pub(crate) const ENV_RETRY_INTERVAL: &str = "HT_RETRY_INTERVAL";
/// The local services, as a TOML array of inline tables.
pub(crate) const ENV_PROXIES: &str = "HT_PROXIES";
/// The token of the next hop, when it differs from the listener token.
pub(crate) const ENV_UPSTREAM_TOKEN: &str = "HT_UPSTREAM_TOKEN";

/// The variables renamed in 2.0, with their replacements. A stale deployment
/// gets an explicit error instead of having the variable silently ignored.
const LEGACY_ENV_VARS: &[(&str, &str)] = &[
    ("HT_SERVER", ENV_UPSTREAM_URL),
    ("HT_SERVER_PORT", ENV_LISTEN_PORT),
    ("HT_SERVER_HOST", ENV_CONTROL_HOST),
];

/// The variables that no longer exist. The listener now always binds every
/// interface, so an old value cannot be honored.
const REMOVED_ENV_VARS: &[&str] = &["HT_BIND_HOST"];

/// The settings that the environment overrides.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct EnvConfig {
    upstream_url: Option<String>,
    name: Option<String>,
    token: Option<MaskedString>,
    listen_port: Option<u16>,
    control_host: Option<String>,
    keepalive_interval: Option<u64>,
    retry_interval: Option<u64>,
    proxies: Option<Vec<ServerProxyEntry>>,
    upstream_token: Option<MaskedString>,
    /// The names of the variables that were set, for a value-free log line
    vars: Vec<&'static str>,
}

impl EnvConfig {
    /// Read the environment of the process.
    pub(crate) fn from_process() -> Result<EnvConfig> {
        EnvConfig::from_lookup(|key| match std::env::var_os(key) {
            None => Ok(None),
            Some(value) => value
                .into_string()
                .map(Some)
                .map_err(|_| anyhow!("The environment variable {} is not valid Unicode", key)),
        })
    }

    /// Read the settings from `lookup`, which maps a variable name to its value.
    pub(crate) fn from_lookup<F>(lookup: F) -> Result<EnvConfig>
    where
        F: Fn(&str) -> Result<Option<String>>,
    {
        let mut env = EnvConfig::default();

        for (old, new) in LEGACY_ENV_VARS {
            if read(&lookup, old)?.is_some() {
                return Err(anyhow!(
                    "The environment variable {} was renamed to {}; update it and restart",
                    old,
                    new
                ));
            }
        }
        for removed in REMOVED_ENV_VARS {
            if read(&lookup, removed)?.is_some() {
                return Err(anyhow!(
                    "The environment variable {} was removed; the listener always binds 0.0.0.0",
                    removed
                ));
            }
        }

        if let Some(value) = read(&lookup, ENV_UPSTREAM_URL)? {
            env.vars.push(ENV_UPSTREAM_URL);
            env.upstream_url = Some(value);
        }
        if let Some(value) = read(&lookup, ENV_NAME)? {
            env.vars.push(ENV_NAME);
            env.name = Some(value);
        }
        if let Some(value) = read(&lookup, ENV_TOKEN)? {
            env.vars.push(ENV_TOKEN);
            env.token = Some(MaskedString::from(value));
        }
        if let Some(value) = read(&lookup, ENV_UPSTREAM_TOKEN)? {
            env.vars.push(ENV_UPSTREAM_TOKEN);
            env.upstream_token = Some(MaskedString::from(value));
        }
        if let Some(value) = read(&lookup, ENV_LISTEN_PORT)? {
            env.vars.push(ENV_LISTEN_PORT);
            env.listen_port = Some(parse_number(ENV_LISTEN_PORT, &value, "a port number")?);
        }
        if let Some(value) = read(&lookup, ENV_CONTROL_HOST)? {
            env.vars.push(ENV_CONTROL_HOST);
            env.control_host = Some(value);
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
        if let Some(value) = read(&lookup, ENV_PROXIES)? {
            env.vars.push(ENV_PROXIES);
            env.proxies = Some(parse_proxies(ENV_PROXIES, &value)?);
        }

        Ok(env)
    }

    /// Overlay the environment on a configuration.
    pub(crate) fn apply(&self, config: &mut Config) {
        if let Some(name) = &self.name {
            config.name = name.clone();
        }
        if let Some(token) = &self.token {
            config.token = Some(token.clone());
        }
        if let Some(listen_port) = self.listen_port {
            config.listen_port = listen_port;
        }
        if let Some(control_host) = &self.control_host {
            config.control_host = Some(control_host.clone());
        }
        if let Some(keepalive_interval) = self.keepalive_interval {
            config.keepalive_interval = keepalive_interval;
        }
        if let Some(retry_interval) = self.retry_interval {
            config.retry_interval = retry_interval;
        }
        // The proxies replace the ones of the file as a whole. Merging would
        // make a removed service linger until the process restarts.
        if let Some(proxies) = &self.proxies {
            config.proxies = proxies.clone();
        }

        if let Some(url) = &self.upstream_url {
            let had_upstream = config.upstream.is_some();
            let upstream = config.upstream.get_or_insert_with(UpstreamConfig::default);
            upstream.url = url.clone();
            if let Some(token) = &self.upstream_token {
                upstream.token = token.clone();
            } else if !had_upstream {
                // An env-only leaf commonly uses one secret on both sides.
                if let Some(token) = &self.token {
                    upstream.token = token.clone();
                }
            }
        } else if let Some(token) = &self.upstream_token {
            if let Some(upstream) = config.upstream.as_mut() {
                upstream.token = token.clone();
            }
        }
    }

    /// Whether the proxies come from `HT_PROXIES` rather than from the file.
    pub(crate) fn proxies_managed(&self) -> bool {
        self.proxies.is_some()
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

/// Read a non-blank variable.
fn read<F>(lookup: &F, key: &str) -> Result<Option<String>>
where
    F: Fn(&str) -> Result<Option<String>>,
{
    lookup(key).map(|value| value.filter(|value| !value.trim().is_empty()))
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

/// The `HT_PROXIES` value, wrapped so that the existing TOML parser reads it.
#[derive(Deserialize)]
struct ProxiesDocument {
    proxies: Vec<ServerProxyEntry>,
}

/// Parse `HT_PROXIES`, e.g.
/// `[{name="nas",domain="nas.example.com",local_addr="127.0.0.1:80"}]`.
fn parse_proxies(key: &str, raw: &str) -> Result<Vec<ServerProxyEntry>> {
    let source = format!("proxies = {}", raw.trim());
    let document: ProxiesDocument = toml::from_str(&source)
        .with_context(|| format!("{} is not a valid TOML array of proxies", key))?;
    Ok(document.proxies)
}

/// Load `ht.toml` and overlay the `HT_*` environment variables.
///
/// A missing file without any `HT_*` uses the first-run bootstrap: the commented
/// example is written and an error asks the user to edit it. A
/// missing file with `HT_*` starts from the defaults instead, so a container
/// needs no mounted configuration. An environment-only configuration never
/// writes `ht.toml`.
pub(crate) async fn load(path: &Path) -> Result<Config> {
    load_with(path, EnvConfig::from_process()?).await
}

async fn load_with(path: &Path, env: EnvConfig) -> Result<Config> {
    let source = Config::read(path).await?;
    let env_only = source.is_none();
    let persisted = match source {
        Some(source) => Config::parse(&source)
            .with_context(|| format!("The configuration {:?} is invalid", path))?,
        None if env.is_empty() => return Err(Config::bootstrap_example(path).await),
        None => Config::default(),
    };

    if !env.is_empty() {
        tracing::info!(
            "Configuration overridden by the environment: {}",
            env.vars().join(", ")
        );
    }
    if env.proxies_managed() {
        tracing::info!(
            "The local services are managed by HT_PROXIES; the web page and the API cannot change them"
        );
    }

    let mut effective = persisted.clone();
    env.apply(&mut effective);
    Config::validate(&mut effective)
        .with_context(|| "The configuration after applying the HT_* environment is invalid")?;
    effective.source = ConfigSource {
        path: Some(path.to_path_buf()),
        persisted: Some(Box::new(persisted)),
        env,
        env_only,
    };

    Ok(effective)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::Result;

    /// Build an environment from pairs, without touching the process one.
    fn env(pairs: &[(&str, &str)]) -> Result<EnvConfig> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect();
        EnvConfig::from_lookup(|key| Ok(map.get(key).cloned()))
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

    const ENV_PROXY: &str = r#"[{name="nas",domain="NAS.Example.com",local_addr="127.0.0.1:80"},
        {name="docker",domain="docker.example.com",local_addr="unix:///var/run/docker.sock",auth_header="secret"}]"#;

    #[test]
    fn test_reads_every_setting() -> Result<()> {
        let parsed = env(&[
            (ENV_UPSTREAM_URL, "wss://control.example.com"),
            (ENV_NAME, "home"),
            (ENV_TOKEN, "secret"),
            (ENV_LISTEN_PORT, "7777"),
            (ENV_CONTROL_HOST, "control.example.com"),
            (ENV_KEEPALIVE_INTERVAL, "5"),
            (ENV_RETRY_INTERVAL, "3"),
            (ENV_PROXIES, ENV_PROXY),
        ])?;

        let mut config = Config::default();
        parsed.apply(&mut config);
        Config::validate(&mut config)?;

        // The canonical form is produced by the same validation as the file
        assert_eq!(
            config.upstream.as_ref().unwrap().url,
            "wss://control.example.com:443"
        );
        assert_eq!(config.name, "home");
        assert_eq!(&**config.token.as_ref().unwrap(), "secret");
        assert_eq!(config.listen_port, 7777);
        assert_eq!(config.control_host(), "control.example.com");
        assert_eq!(config.keepalive_interval, 5);
        assert_eq!(config.retry_interval, 3);

        assert_eq!(config.proxies.len(), 2);
        assert_eq!(config.proxies[0].name, "nas");
        assert_eq!(config.proxies[0].local_addr, "127.0.0.1:80");
        assert_eq!(&**config.proxies[1].auth_header.as_ref().unwrap(), "secret");
        assert_eq!(config.proxies[1].domain, "docker.example.com");

        assert_eq!(
            parsed.vars().to_vec(),
            vec![
                ENV_UPSTREAM_URL,
                ENV_NAME,
                ENV_TOKEN,
                ENV_LISTEN_PORT,
                ENV_CONTROL_HOST,
                ENV_KEEPALIVE_INTERVAL,
                ENV_RETRY_INTERVAL,
                ENV_PROXIES,
            ]
        );
        assert!(parsed.proxies_managed());
        Ok(())
    }

    #[test]
    fn test_blank_values_are_ignored() -> Result<()> {
        let parsed = env(&[(ENV_NAME, "  "), (ENV_TOKEN, ""), (ENV_PROXIES, "\n")])?;
        assert!(parsed.is_empty());
        assert!(!parsed.proxies_managed());
        assert_eq!(parsed, EnvConfig::default());
        Ok(())
    }

    #[test]
    fn test_legacy_variables_are_rejected() {
        for &(old, new) in LEGACY_ENV_VARS {
            let err = format!("{:#}", env(&[(old, "value")]).unwrap_err());
            assert!(err.contains(old), "{}", err);
            assert!(err.contains(new), "{}", err);
        }
    }

    #[test]
    fn test_blank_legacy_variables_are_ignored() -> Result<()> {
        let parsed = env(&[
            ("HT_SERVER", " "),
            ("HT_SERVER_PORT", ""),
            ("HT_SERVER_HOST", "\n"),
        ])?;
        assert!(parsed.is_empty());
        Ok(())
    }

    #[test]
    fn test_removed_variables_are_rejected() {
        for &removed in REMOVED_ENV_VARS {
            let err = format!("{:#}", env(&[(removed, "127.0.0.1")]).unwrap_err());
            assert!(err.contains(removed), "{}", err);
            assert!(err.contains("removed"), "{}", err);
        }
    }

    #[test]
    fn test_invalid_values_name_the_variable() {
        let err = format!("{:#}", env(&[(ENV_RETRY_INTERVAL, "-1")]).unwrap_err());
        assert!(err.contains(ENV_RETRY_INTERVAL), "{}", err);

        let err = format!("{:#}", env(&[(ENV_PROXIES, "{oops")]).unwrap_err());
        assert!(err.contains(ENV_PROXIES), "{}", err);

        // An unknown proxy field must not be silently discarded
        let err = format!(
            "{:#}",
            env(&[(
                ENV_PROXIES,
                r#"[{name="x",domain="x.example.com",local_addr="1.2.3.4:5",oops=1}]"#
            )])
            .unwrap_err()
        );
        assert!(err.contains(ENV_PROXIES), "{}", err);
    }

    #[test]
    fn test_scalar_override_keeps_the_file_proxies() -> Result<()> {
        let mut config = Config::parse(
            r#"
control_host = "control.example.com"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let parsed = env(&[(ENV_TOKEN, "env-token"), (ENV_NAME, "container")])?;
        parsed.apply(&mut config);
        Config::validate(&mut config)?;

        assert_eq!(&**config.token.as_ref().unwrap(), "env-token");
        assert_eq!(config.proxies.len(), 1);
        assert_eq!(config.proxies[0].name, "nas");
        assert!(!parsed.proxies_managed());
        Ok(())
    }

    #[test]
    fn test_proxies_replace_the_file_ones() -> Result<()> {
        let mut config = Config::parse(
            r#"
control_host = "control.example.com"
token = "secret"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let parsed = env(&[(
            ENV_PROXIES,
            r#"[{name="git",domain="git.example.com",local_addr="127.0.0.1:81"}]"#,
        )])?;
        parsed.apply(&mut config);
        Config::validate(&mut config)?;

        assert_eq!(config.proxies.len(), 1);
        assert_eq!(config.proxies[0].name, "git");
        Ok(())
    }

    #[test]
    fn test_an_empty_array_clears_the_proxies() -> Result<()> {
        let parsed = env(&[(ENV_PROXIES, "[]")])?;
        assert!(parsed.proxies_managed());

        let mut config = Config::parse(
            r#"
control_host = "control.example.com"
token = "secret"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;
        parsed.apply(&mut config);
        Config::validate(&mut config)?;
        assert!(config.proxies.is_empty());
        Ok(())
    }

    #[test]
    fn test_duplicate_names_and_domains_are_rejected() -> Result<()> {
        for proxies in [
            r#"[{name="nas",domain="a.example.com",local_addr="127.0.0.1:80"},
                {name="nas",domain="b.example.com",local_addr="127.0.0.1:81"}]"#,
            r#"[{name="a",domain="same.example.com",local_addr="127.0.0.1:80"},
                {name="b",domain="SAME.example.com",local_addr="127.0.0.1:81"}]"#,
        ] {
            let parsed = env(&[
                (ENV_CONTROL_HOST, "control.example.com"),
                (ENV_TOKEN, "secret"),
                (ENV_PROXIES, proxies),
            ])?;
            let mut config = Config::default();
            parsed.apply(&mut config);
            let err = format!("{:#}", Config::validate(&mut config).unwrap_err());
            assert!(err.contains("more than once"), "{}", err);
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_load_without_a_file_uses_the_environment() -> Result<()> {
        let path = temp_path("env-only");
        let env = env(&[
            (ENV_UPSTREAM_URL, "ws://127.0.0.1:8080"),
            (ENV_NAME, "home"),
            (ENV_TOKEN, "secret"),
            (ENV_PROXIES, ENV_PROXY),
        ])?;

        let loaded = load_with(&path, env).await?;
        assert!(!path.exists(), "the example file must not be generated");
        assert_eq!(loaded.source.persisted.as_deref(), Some(&Config::default()));
        assert_eq!(loaded.proxies.len(), 2);
        assert!(loaded.source.env.proxies_managed());
        assert!(loaded.source.env_only);
        Ok(())
    }

    #[tokio::test]
    async fn test_load_without_a_file_and_without_the_environment_writes_the_example() -> Result<()>
    {
        let path = temp_path("bootstrap");
        let err = load_with(&path, EnvConfig::default())
            .await
            .expect_err("the first start must ask the user to edit the example");
        assert!(
            err.downcast_ref::<crate::GeneratedConfigError>().is_some(),
            "the first start must return the bootstrap marker: {:#}",
            err
        );
        assert!(format!("{:#}", err).contains("已生成"), "{}", err);
        let example = std::fs::read_to_string(&path)?;
        let _ = std::fs::remove_file(&path);
        assert!(example.contains("[[proxies]]"), "{}", example);
        assert!(example.contains("[upstream]"), "{}", example);
        Ok(())
    }

    #[tokio::test]
    async fn test_load_prefers_the_environment_and_keeps_the_file_baseline() -> Result<()> {
        let path = temp_path("baseline");
        std::fs::write(
            &path,
            r#"
control_host = "control.example.com"
name = "file"
token = "file-token"

[[proxies]]
name = "nas"
domain = "nas.example.com"
local_addr = "127.0.0.1:80"
"#,
        )?;

        let env = env(&[(ENV_TOKEN, "env-token"), (ENV_PROXIES, ENV_PROXY)])?;
        let loaded = load_with(&path, env).await?;
        let _ = std::fs::remove_file(&path);

        // Runtime uses the environment
        assert_eq!(&**loaded.token.as_ref().unwrap(), "env-token");
        assert_eq!(loaded.proxies.len(), 2);

        // The baseline is what the API may write back
        let persisted = loaded.source.persisted.as_deref().unwrap();
        assert_eq!(&**persisted.token.as_ref().unwrap(), "file-token");
        assert_eq!(persisted.proxies.len(), 1);
        assert_eq!(persisted.proxies[0].name, "nas");
        Ok(())
    }
}
