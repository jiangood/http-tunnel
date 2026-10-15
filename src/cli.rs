use anyhow::Result;
use clap::{AppSettings, Args, Parser, Subcommand};
use lazy_static::lazy_static;
use std::path::PathBuf;

// `build.rs` emits the `VERGEN_*` variables as `cargo:rustc-env`, so they are
// available to every target of the crate. They are read with `option_env!` and
// fall back to `unknown` so that a consumer of the library still builds if a
// variable is ever missing. The git-derived
// variables are not reported because `build.rs` leaves the `git` feature of
// `vergen` disabled (see `build.rs`).
macro_rules! build_env {
    ($name:literal) => {
        option_env!($name).unwrap_or("unknown")
    };
}

lazy_static! {
    static ref VERSION: &'static str = build_env!("VERGEN_BUILD_SEMVER");
    static ref LONG_VERSION: String = format!(
        "
Build Timestamp:     {}
Build Version:       {}
cargo Target Triple: {}
cargo Profile:       {}
cargo Features:      {}
",
        build_env!("VERGEN_BUILD_TIMESTAMP"),
        build_env!("VERGEN_BUILD_SEMVER"),
        build_env!("VERGEN_CARGO_TARGET_TRIPLE"),
        build_env!("VERGEN_CARGO_PROFILE"),
        build_env!("VERGEN_CARGO_FEATURES")
    );
}

// The command line. The client is the default role, so with no subcommand the
// top-level options configure it; `server` is the only explicit subcommand.
#[derive(Parser, Debug, Clone)]
#[clap(
    about,
    version(*VERSION),
    long_version(LONG_VERSION.as_str()),
    setting(AppSettings::DeriveDisplayOrder)
)]
pub struct Cli {
    // The client options, used when no `server` subcommand is given.
    #[clap(flatten)]
    client: ClientOptions,

    #[clap(subcommand)]
    cmd: Option<ServerCommand>,
}

// The only explicit subcommand. The client is the default role, so it has no
// subcommand of its own.
#[derive(Subcommand, Debug, Clone)]
enum ServerCommand {
    /// Run as a server. It takes no configuration file: the tunnels are declared
    /// by the clients in their own configuration files
    Server(ServerArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// The port that the server listens on for clients (control and data channels)
    ///
    /// It can also be passed via the `HTTP_TUNNEL_SERVER_PORT` environment variable
    #[clap(long, env = "HTTP_TUNNEL_SERVER_PORT", default_value_t = 2333)]
    pub server_port: u16,

    /// The HTTP entrypoint. Visitors are routed by the `Host` header
    ///
    /// It can also be passed via the `HTTP_TUNNEL_HTTP_PORT` environment variable
    #[clap(long, env = "HTTP_TUNNEL_HTTP_PORT", default_value_t = 80)]
    pub http_port: u16,

    /// The shared token every client must authenticate with
    ///
    /// It can also be passed via the `HTTP_TUNNEL_TOKEN` environment variable,
    /// which keeps it out of `ps`
    #[clap(long, env = "HTTP_TUNNEL_TOKEN", hide_env_values = true)]
    pub token: String,
}

// The client options as parsed from the top level. The identity values are
// optional here: they can also come from the configuration file, and
// `run_client` merges the two.
#[derive(Args, Debug, Clone)]
struct ClientOptions {
    /// The address of the server, e.g. `example.com:2333`
    ///
    /// It can also be passed via the `HTTP_TUNNEL_REMOTE` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_REMOTE")]
    remote: Option<String>,

    /// The name of the client, used for logging
    ///
    /// It can also be passed via the `HTTP_TUNNEL_NAME` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_NAME")]
    name: Option<String>,

    /// The shared token of the server
    ///
    /// It can also be passed via the `HTTP_TUNNEL_TOKEN` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// The port of the client administration API. Overrides the configuration
    /// file and defaults to `8610`.
    ///
    /// It listens on all interfaces (`0.0.0.0`) and requires
    /// `Authorization: Bearer <token>`. It's only compiled in with the
    /// `client-api` feature (on by default).
    ///
    /// Set it to `0` to disable the API, which is useful when several clients
    /// run on the same host. It can also be passed via the
    /// `HTTP_TUNNEL_API_PORT` environment variable
    #[clap(long, env = "HTTP_TUNNEL_API_PORT")]
    api_port: Option<u16>,

    /// The path to the configuration file (defaults to `client.toml` in the
    /// current directory)
    #[clap(long = "config", short = 'c', parse(from_os_str), default_value = "client.toml")]
    config_path: PathBuf,
}

// The client options once the command line is parsed. The identity values stay
// optional: they are resolved against the configuration file in `run_client`.
#[derive(Debug, Clone)]
pub struct ClientArgs {
    pub config_path: PathBuf,
    pub remote: Option<String>,
    pub name: Option<String>,
    pub token: Option<String>,
    pub api_port: Option<u16>,
}

/// The default port of the client administration API
pub const DEFAULT_CLIENT_API_PORT: u16 = 8610;

// The role selected on the command line, resolved from the parsed `Cli`.
#[derive(Debug, Clone)]
pub enum Command {
    Server(ServerArgs),
    Client(ClientArgs),
}

impl Cli {
    /// Resolve the command line into the role to run. The client is the default
    /// role: when no `server` subcommand is given, the top-level options
    /// configure a client, whose identity is merged with its configuration file
    /// in `run_client`.
    pub fn into_command(self) -> Result<Command> {
        match self.cmd {
            Some(ServerCommand::Server(args)) => Ok(Command::Server(args)),
            None => {
                let ClientOptions {
                    remote,
                    name,
                    token,
                    api_port,
                    config_path,
                } = self.client;
                Ok(Command::Client(ClientArgs {
                    config_path,
                    remote,
                    name,
                    token,
                    api_port,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_subcommand_is_a_client() {
        let cli = Cli::try_parse_from([
            "http-tunnel",
            "--remote",
            "example.com:2333",
            "--name",
            "home",
            "--token",
            "secret",
        ])
        .unwrap();

        match cli.into_command().unwrap() {
            Command::Client(args) => {
                assert_eq!(args.remote.as_deref(), Some("example.com:2333"));
                assert_eq!(args.name.as_deref(), Some("home"));
                assert_eq!(args.token.as_deref(), Some("secret"));
                // The administration API port is left to the configuration file
                assert_eq!(args.api_port, None);
                // The configuration defaults to `client.toml`
                assert_eq!(args.config_path, PathBuf::from("client.toml"));
            }
            Command::Server(_) => panic!("expected the default role to be a client"),
        }
    }

    #[test]
    fn the_client_config_path_can_be_changed() {
        let cli = Cli::try_parse_from([
            "http-tunnel",
            "--remote",
            "example.com:2333",
            "--name",
            "home",
            "--token",
            "secret",
            "--config",
            "custom.toml",
        ])
        .unwrap();

        match cli.into_command().unwrap() {
            Command::Client(args) => assert_eq!(args.config_path, PathBuf::from("custom.toml")),
            Command::Server(_) => panic!("expected the default role to be a client"),
        }
    }

    #[test]
    fn the_client_can_be_started_without_identity_flags() {
        // The identity may come from the configuration file, so the command line
        // doesn't enforce it
        let cli = Cli::try_parse_from(["http-tunnel"]).unwrap();
        match cli.into_command().unwrap() {
            Command::Client(args) => {
                assert_eq!(args.remote, None);
                assert_eq!(args.name, None);
                assert_eq!(args.token, None);
            }
            Command::Server(_) => panic!("expected the default role to be a client"),
        }
    }

    #[test]
    fn the_server_subcommand_wins_over_the_client_options() {
        let cli =
            Cli::try_parse_from(["http-tunnel", "server", "--server-port", "3000", "--token", "s"])
                .unwrap();

        match cli.into_command().unwrap() {
            Command::Server(args) => {
                assert_eq!(args.server_port, 3000);
                assert_eq!(args.http_port, 80);
                assert_eq!(args.token, "s");
            }
            Command::Client(_) => panic!("expected a server"),
        }
    }

    #[test]
    fn parsing_the_server_subcommand_defaults() {
        let cli = Cli::try_parse_from(["http-tunnel", "server", "--token", "s"]).unwrap();
        match cli.into_command().unwrap() {
            Command::Server(args) => {
                assert_eq!(args.server_port, 2333);
                assert_eq!(args.http_port, 80);
            }
            Command::Client(_) => panic!("expected a server"),
        }
    }
}
