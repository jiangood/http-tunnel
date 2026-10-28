use std::path::PathBuf;

/// Default port for the client web management interface.
pub const DEFAULT_CLIENT_WEB_PORT: u16 = 8610;

/// Server runtime settings loaded from hts.toml.
#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub server_port: u16,
    pub server_host: String,
    /// The IP address the HTTP entrypoint binds
    pub bind_host: String,
    pub token: String,
    pub proxies: Vec<crate::config::ServerProxyEntry>,
    pub limits: crate::config::ServerLimits,
}

impl ServerArgs {
    /// Create the arguments with the default limits.
    pub fn new(
        server_port: u16,
        server_host: String,
        token: String,
        proxies: Vec<crate::config::ServerProxyEntry>,
    ) -> ServerArgs {
        ServerArgs {
            server_port,
            server_host,
            bind_host: crate::helper::DEFAULT_BIND_HOST.to_string(),
            token,
            proxies,
            limits: crate::config::ServerLimits::default(),
        }
    }
}

/// Client runtime settings. The configuration path is fixed to htc.toml; the
/// `HTC_*` environment variables override the settings of that file.
#[derive(Debug, Clone)]
pub struct ClientArgs {
    pub config_path: PathBuf,
}

/// Node runtime settings. The configuration path is fixed to htn.toml.
#[derive(Debug, Clone)]
pub struct NodeArgs {
    pub config: crate::config::NodeConfig,
}

#[derive(Debug, Clone)]
pub enum Command {
    Server(ServerArgs),
    Client(ClientArgs),
    Node(NodeArgs),
}
