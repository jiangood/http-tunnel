use std::path::PathBuf;

/// Default port for the client web management interface.
pub const DEFAULT_CLIENT_WEB_PORT: u16 = 8610;

/// Server runtime settings loaded from hts.toml.
#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub http_port: u16,
    pub control_host: String,
    pub token: String,
    pub proxies: Vec<crate::config::ServerProxyEntry>,
    pub limits: crate::config::ServerLimits,
}

impl ServerArgs {
    /// Create the arguments with the default limits.
    pub fn new(
        http_port: u16,
        control_host: String,
        token: String,
        proxies: Vec<crate::config::ServerProxyEntry>,
    ) -> ServerArgs {
        ServerArgs {
            http_port,
            control_host,
            token,
            proxies,
            limits: crate::config::ServerLimits::default(),
        }
    }
}

/// Client runtime settings. The configuration path is fixed to htc.toml.
#[derive(Debug, Clone)]
pub struct ClientArgs {
    pub config_path: PathBuf,
}

#[derive(Debug, Clone)]
pub enum Command {
    Server(ServerArgs),
    Client(ClientArgs),
}
