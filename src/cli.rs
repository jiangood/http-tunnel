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

#[derive(Parser, Debug, Clone)]
#[clap(
    about,
    version(*VERSION),
    long_version(LONG_VERSION.as_str()),
    setting(AppSettings::DeriveDisplayOrder)
)]
pub struct Cli {
    #[clap(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Run as a server. The configuration file defines all the clients and their tunnels
    Server(ServerArgs),
    /// Run as a client. The client is configured by the server, so it takes no configuration file
    Client(ClientArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// The path to the configuration file (defaults to `server.toml` in the current directory)
    #[clap(parse(from_os_str), name = "CONFIG", default_value = "server.toml")]
    pub config_path: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ClientArgs {
    /// The address of the server, e.g. `example.com:2333`
    ///
    /// It can also be passed via the `HTTP_TUNNEL_REMOTE` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_REMOTE")]
    pub remote: String,

    /// The name of the client. It must be defined in the server's configuration
    ///
    /// It can also be passed via the `HTTP_TUNNEL_NAME` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_NAME")]
    pub name: String,

    /// The token of the client. It must match the one in the server's configuration
    ///
    /// It can also be passed via the `HTTP_TUNNEL_TOKEN` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_TOKEN", hide_env_values = true)]
    pub token: String,

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
    pub api_port: Option<u16>,
}

/// The default port of the client administration API
pub const DEFAULT_CLIENT_API_PORT: u16 = 8610;

impl Cli {
    /// Fill the defaults that clap can't express on an `Option` field. The client
    /// administration API listens on `DEFAULT_CLIENT_API_PORT` unless a port is
    /// given on the command line or through `HTTP_TUNNEL_API_PORT`.
    pub fn apply_client_defaults(&mut self) {
        if let Command::Client(args) = &mut self.cmd {
            args.api_port.get_or_insert(DEFAULT_CLIENT_API_PORT);
        }
    }
}
