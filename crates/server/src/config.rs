use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;
use figment::Figment;
use figment::providers::{Format, Serialized, Yaml};
use serde::{Deserialize, Serialize};

/// Config file consulted when `--config` is not passed. Relative to the working
/// directory, which is the workspace root under `cargo run`.
const DEFAULT_CONFIG_PATH: &str = "config/server.yaml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub ris: RisConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub port: u16,
    /// Directory served for any request that is not an API or websocket route.
    /// Superseded later once the client is embedded in the binary.
    pub assets_dir: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// `tracing_subscriber::EnvFilter` directive, e.g. `info,slash0=debug`.
    pub filter: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RisConfig {
    /// RIS Live route collector (RRC) hostname to consume from. Every peer that
    /// collector sees is ingested (announcements and withdrawals both).
    pub host: String,
    /// Seed the trie with data from an RIS data dump file. Can be a URL, but server
    /// startup will be way faster with a file. File can be gzip'd. Optional,
    /// if omitted the server isn't seeded and accumulates routes slooowly
    /// from RIS live alone.
    ///
    /// Download files from: https://ris.ripe.net/docs/mrt/
    pub seed_file: Option<String>,

    /// File with newline separated JSON RIS messages (for offline development). These
    /// will just be looped forever
    pub mock_events_file: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 3000,
            assets_dir: PathBuf::from("crates/client/dist"),
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "info,slash0=debug,tower_http=debug".to_owned(),
        }
    }
}

impl Default for RisConfig {
    fn default() -> Self {
        Self {
            host: "rrc00.ripe.net".to_owned(),
            seed_file: None,
            mock_events_file: None,
        }
    }
}

impl ServerConfig {
    pub fn socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.port)
    }
}

/// Layers defaults under the YAML file (if any). An explicit `--config` path
/// that does not exist is an error; the default path is optional so a fresh
/// checkout runs on defaults alone.
pub fn load(explicit_path: Option<PathBuf>) -> anyhow::Result<Config> {
    let (path, required) = match explicit_path {
        Some(path) => (path, true),
        None => (PathBuf::from(DEFAULT_CONFIG_PATH), false),
    };

    if required && !path.exists() {
        anyhow::bail!("config file not found: {}", path.display());
    }

    Figment::new()
        .merge(Serialized::defaults(Config::default()))
        .merge(Yaml::file(&path))
        .extract()
        .with_context(|| format!("failed to load config from {}", path.display()))
}
