use clap::{AppSettings, Args, Parser, Subcommand};
use lazy_static::lazy_static;
use std::path::PathBuf;

lazy_static! {
    static ref VERSION: &'static str =
        option_env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT").unwrap_or(env!("VERGEN_BUILD_SEMVER"));
    static ref LONG_VERSION: String = format!(
        "
Build Timestamp:     {}
Build Version:       {}
Commit SHA:          {:?}
Commit Date:         {:?}
Commit Branch:       {:?}
cargo Target Triple: {}
cargo Profile:       {}
cargo Features:      {}
",
        env!("VERGEN_BUILD_TIMESTAMP"),
        env!("VERGEN_BUILD_SEMVER"),
        option_env!("VERGEN_GIT_SHA"),
        option_env!("VERGEN_GIT_COMMIT_TIMESTAMP"),
        option_env!("VERGEN_GIT_BRANCH"),
        env!("VERGEN_CARGO_TARGET_TRIPLE"),
        env!("VERGEN_CARGO_PROFILE"),
        env!("VERGEN_CARGO_FEATURES")
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
    /// Run as a server. The configuration file defines all the clients and their services
    Server(ServerArgs),
    /// Run as a client. The client is configured by the server, so it takes no configuration file
    Client(ClientArgs),
}

#[derive(Args, Debug, Clone)]
pub struct ServerArgs {
    /// The path to the configuration file
    #[clap(parse(from_os_str), name = "CONFIG")]
    pub config_path: PathBuf,
}

#[derive(Args, Debug, Clone)]
pub struct ClientArgs {
    /// The address of the server, e.g. `example.com:2333`
    #[clap(long, short)]
    pub remote: String,

    /// The name of the client. It must be defined in the server's configuration
    #[clap(long, short)]
    pub name: String,

    /// The token of the client. It must match the one in the server's configuration
    ///
    /// It can also be passed via the `HTTP_TUNNEL_TOKEN` environment variable
    #[clap(long, short, env = "HTTP_TUNNEL_TOKEN", hide_env_values = true)]
    pub token: String,
}
