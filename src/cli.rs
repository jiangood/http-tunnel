use anyhow::Result;
use clap::{AppSettings, Args, Parser, Subcommand};
use lazy_static::lazy_static;
use std::path::PathBuf;

// `build.rs` emits the `VERGEN_*` variables as `cargo:rustc-env`, so they are
// available to every target of the crate. They are read with `option_env!` and
// fall back to `unknown` so that the mobile `cdylib` (and any other consumer of
// the library) still builds if a variable is ever missing. The git-derived
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
    // The client options, used when no `server` subcommand is given. They are
    // all optional here because clap can't express "required unless a
    // subcommand is given"; `into_command` enforces them for the default role.
    #[clap(flatten)]
    client: ClientOptions,

    #[clap(subcommand)]
    cmd: Option<ServerCommand>,
}

// The only explicit subcommand. The client is the default role, so it has no
// subcommand of its own.
#[derive(Subcommand, Debug, Clone)]
enum ServerCommand {
    /// Run as a server. The configuration file defines all the clients and their tunnels
    Server(ServerArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// The path to the configuration file (defaults to `server.toml` in the current directory)
    #[clap(parse(from_os_str), name = "CONFIG", default_value = "server.toml")]
    pub config_path: PathBuf,
}

// The client options as parsed from the top level. They mirror `ClientArgs`, but
// every field is optional: the values are only required when the client is the
// role actually selected (i.e. no `server` subcommand), which `into_command`
// checks.
#[derive(Args, Debug, Clone)]
struct ClientOptions {
    /// The address of the server, e.g. `example.com:2333`
    ///
    /// It can also be passed via the `HTTP_TUNNEL_REMOTE` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_REMOTE")]
    remote: Option<String>,

    /// The name of the client. It must be defined in the server's configuration
    ///
    /// It can also be passed via the `HTTP_TUNNEL_NAME` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_NAME")]
    name: Option<String>,

    /// The token of the client. It must match the one in the server's configuration
    ///
    /// It can also be passed via the `HTTP_TUNNEL_TOKEN` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_TOKEN", hide_env_values = true)]
    token: Option<String>,

    /// The port of the client administration API. Defaults to `8610`.
    ///
    /// It listens on all interfaces (`0.0.0.0`) and requires
    /// `Authorization: Bearer <token>`. It's only compiled in with the
    /// `client-api` feature (on by default).
    ///
    /// Set it to `0` to disable the API, which is useful when several clients
    /// run on the same host. It can also be passed via the
    /// `HTTP_TUNNEL_API_PORT` environment variable, which is convenient to start
    /// a containerized client
    #[clap(long, env = "HTTP_TUNNEL_API_PORT")]
    api_port: Option<u16>,
}

// The client options once the client is known to be the selected role: every
// value is present. This is what `run_client` consumes.
#[derive(Debug, Clone)]
pub struct ClientArgs {
    pub remote: String,
    pub name: String,
    pub token: String,
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
    /// configure a client, and the client administration API listens on
    /// `DEFAULT_CLIENT_API_PORT` unless a port is given.
    pub fn into_command(self) -> Result<Command> {
        match self.cmd {
            Some(ServerCommand::Server(args)) => Ok(Command::Server(args)),
            None => {
                let ClientOptions {
                    remote,
                    name,
                    token,
                    mut api_port,
                } = self.client;

                let mut missing = Vec::new();
                if remote.is_none() {
                    missing.push("--remote");
                }
                if name.is_none() {
                    missing.push("--name");
                }
                if token.is_none() {
                    missing.push("--token");
                }
                if !missing.is_empty() {
                    anyhow::bail!(
                        "the client is the default role and the following required \
                         arguments were not provided: {}\n\n\
                         They can also be passed through the `HTTP_TUNNEL_REMOTE`, \
                         `HTTP_TUNNEL_NAME` and `HTTP_TUNNEL_TOKEN` environment \
                         variables, or run a server with the `server` subcommand",
                        missing.join(", ")
                    );
                }

                api_port.get_or_insert(DEFAULT_CLIENT_API_PORT);
                Ok(Command::Client(ClientArgs {
                    remote: remote.expect("checked above"),
                    name: name.expect("checked above"),
                    token: token.expect("checked above"),
                    api_port,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_options(
        remote: Option<&str>,
        name: Option<&str>,
        token: Option<&str>,
        api_port: Option<u16>,
    ) -> ClientOptions {
        ClientOptions {
            remote: remote.map(str::to_owned),
            name: name.map(str::to_owned),
            token: token.map(str::to_owned),
            api_port,
        }
    }

    #[test]
    fn no_subcommand_is_a_client() {
        let cli = Cli {
            client: client_options(Some("example.com:2333"), Some("home"), Some("secret"), None),
            cmd: None,
        };

        match cli.into_command().unwrap() {
            Command::Client(args) => {
                assert_eq!(args.remote, "example.com:2333");
                assert_eq!(args.name, "home");
                assert_eq!(args.token, "secret");
                // The administration API defaults to `DEFAULT_CLIENT_API_PORT`
                assert_eq!(args.api_port, Some(DEFAULT_CLIENT_API_PORT));
            }
            Command::Server(_) => panic!("expected the default role to be a client"),
        }
    }

    #[test]
    fn an_explicit_port_is_kept() {
        let cli = Cli {
            client: client_options(
                Some("example.com:2333"),
                Some("home"),
                Some("secret"),
                Some(0),
            ),
            cmd: None,
        };

        match cli.into_command().unwrap() {
            Command::Client(args) => assert_eq!(args.api_port, Some(0)),
            Command::Server(_) => panic!("expected the default role to be a client"),
        }
    }

    #[test]
    fn the_server_subcommand_wins_over_the_client_options() {
        let cli = Cli {
            client: client_options(None, None, None, None),
            cmd: Some(ServerCommand::Server(ServerArgs {
                config_path: PathBuf::from("server.toml"),
            })),
        };

        match cli.into_command().unwrap() {
            Command::Server(args) => assert_eq!(args.config_path, PathBuf::from("server.toml")),
            Command::Client(_) => panic!("expected a server"),
        }
    }

    #[test]
    fn the_default_client_requires_the_address_the_name_and_the_token() {
        let cli = Cli {
            client: client_options(Some("example.com:2333"), Some("home"), None, None),
            cmd: None,
        };

        let err = cli.into_command().unwrap_err();
        assert!(err.to_string().contains("--token"));
    }

    #[test]
    fn the_parsed_flags_become_the_default_client() {
        let cli = Cli::try_parse_from([
            "http-tunnel",
            "--remote",
            "example.com:2333",
            "--name",
            "home",
            "--token",
            "secret",
            "--api-port",
            "8611",
        ])
        .unwrap();

        match cli.into_command().unwrap() {
            Command::Client(args) => {
                assert_eq!(args.remote, "example.com:2333");
                assert_eq!(args.name, "home");
                assert_eq!(args.token, "secret");
                assert_eq!(args.api_port, Some(8611));
            }
            Command::Server(_) => panic!("expected a client"),
        }
    }

    #[test]
    fn parsing_the_server_subcommand() {
        let cli = Cli::try_parse_from(["http-tunnel", "server", "custom.toml"]).unwrap();

        match cli.into_command().unwrap() {
            Command::Server(args) => assert_eq!(args.config_path, PathBuf::from("custom.toml")),
            Command::Client(_) => panic!("expected a server"),
        }
    }
}
