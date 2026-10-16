use std::path::PathBuf;

/// Default port for the client web management interface.
pub const DEFAULT_CLIENT_WEB_PORT: u16 = 8610;

/// Server runtime settings loaded from hts.toml.
#[derive(Debug, Clone)]
pub struct ServerArgs {
    pub server_port: u16,
    pub http_port: u16,
    pub token: String,
    pub proxies: Vec<crate::config::ServerProxyEntry>,
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
