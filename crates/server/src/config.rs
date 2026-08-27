use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;

use anyhow::Context;
use figment::providers::{Format, Serialized, Yaml};
use figment::{Figment, Provider};
use serde::{Deserialize, Serialize};

/// Config file consulted when `--config` is not passed. Relative to the working
/// directory, which is the workspace root under `cargo run`.
const DEFAULT_CONFIG_PATH: &str = "config/server.yaml";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub logging: LoggingConfig,
    pub ris: RisConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: IpAddr,
    pub http_port: u16,
    /// Directory served for any request that is not an API or websocket route.
    /// Superseded later once the client is embedded in the binary.
    pub assets_dir: PathBuf,
    pub tls_config: Option<TlsConfig>,
}

/// Opting in to TLS makes every field below required, so a half-written block
/// fails at startup rather than silently serving plain HTTP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TlsConfig {
    pub https_port: u16,
    pub lets_encrypt: LetsEncryptConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LetsEncryptConfig {
    /// Every domain the certificate should cover. Let's Encrypt validates each
    /// one over HTTP-01, so all of them must resolve to this server on port 80.
    pub domains: Vec<String>,
    /// Holds the ACME account key and the issued certificate. Must be writable
    /// and must persist across restarts, or every boot re-orders from scratch.
    pub certs_dir: PathBuf,
    /// The staging directory has far looser rate limits but issues certificates
    /// browsers do not trust. Leave false until a staging order is seen to succeed.
    pub prod_letsencrypt: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    /// `tracing_subscriber::EnvFilter` directive, e.g. `info,slash0=debug`.
    pub filter: String,
    pub format: LogFormat,
    /// Kept independent of the sink: colour codes are readable under `less -R`
    /// too, so writing to a file is not on its own a reason to drop them.
    pub ansi: bool,
    /// When set, logs go to rotating files here *instead of* stdout.
    pub file: Option<FileLoggingConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Compact,
    Pretty,
}

/// Opting in to file logging makes `dir` required, following the same reasoning
/// as [`TlsConfig`]: a half-written block should fail at startup rather than
/// quietly write somewhere unexpected. The rest have production-appropriate
/// defaults, so `file: {dir: /var/log/slash0}` is a complete block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileLoggingConfig {
    /// The live log is `<dir>/<prefix>.log`; rotated files get a `.<timestamp>`
    /// suffix, plus `.gz` once compressed.
    pub dir: PathBuf,
    #[serde(default = "default_log_prefix")]
    pub prefix: String,
    /// Rotate once the live file grows past this.
    #[serde(default = "default_max_file_size_mb")]
    pub max_file_size_mb: u64,
    /// Rotated files retained before the oldest is deleted. Together with
    /// `max_file_size_mb` this bounds total disk usage at roughly
    /// `max_file_size_mb * (max_files + 1)`, and well under it once compression
    /// applies.
    #[serde(default = "default_max_log_files")]
    pub max_files: usize,
    /// How many of the most recent rotated files to leave uncompressed before
    /// gzipping the rest -- the newest rotation is usually the one being
    /// grepped. `null` disables compression entirely.
    #[serde(default = "default_keep_uncompressed")]
    pub keep_uncompressed: Option<usize>,
}

fn default_log_prefix() -> String {
    "slash0".to_owned()
}

fn default_max_file_size_mb() -> u64 {
    64
}

fn default_max_log_files() -> usize {
    14
}

fn default_keep_uncompressed() -> Option<usize> {
    Some(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

    pub mock_stream_config: Option<MockStreamConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MockStreamConfig {
    /// File with newline separated JSON RIS messages (for offline development)
    pub mock_events_file: String,

    /// If true, wait between "receiving" each event based on the delay between timestamps in the
    /// recorded event
    pub simulate_message_rate: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            http_port: 3000,
            assets_dir: PathBuf::from("crates/client/dist"),
            tls_config: None,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            filter: "info,slash0=debug,tower_http=debug".to_owned(),
            format: LogFormat::Compact,
            ansi: true,
            file: None,
        }
    }
}

impl Default for RisConfig {
    fn default() -> Self {
        Self {
            host: "rrc00.ripe.net".to_owned(),
            seed_file: None,
            mock_stream_config: None,
        }
    }
}

impl ServerConfig {
    pub fn http_socket_addr(&self) -> SocketAddr {
        SocketAddr::new(self.host, self.http_port)
    }

    pub fn https_socket_addr(&self) -> Option<SocketAddr> {
        Some(SocketAddr::new(
            self.host,
            self.tls_config.as_ref()?.https_port,
        ))
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

    extract(Yaml::file(&path))
        .with_context(|| format!("failed to load config from {}", path.display()))
}

fn extract(provider: impl Provider) -> anyhow::Result<Config> {
    let config: Config = Figment::new()
        .merge(Serialized::defaults(Config::default()))
        .merge(provider)
        .extract()?;
    config.validate()?;
    Ok(config)
}

impl Config {
    /// Catches values that parse but cannot be honoured, so they surface here
    /// rather than deeper in the code that consumes them.
    fn validate(&self) -> anyhow::Result<()> {
        if let Some(file) = &self.logging.file {
            // `FileRotate` panics outright on a zero-byte rotation limit.
            anyhow::ensure!(
                file.max_file_size_mb > 0,
                "logging.file.max_file_size_mb must be greater than zero"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use figment::providers::Yaml;

    fn load_yaml(yaml: &str) -> anyhow::Result<Config> {
        extract(Yaml::string(yaml))
    }

    #[test]
    fn tls_is_off_when_unconfigured() {
        let config = load_yaml("server:\n  http_port: 8080\n").unwrap();
        assert!(config.server.tls_config.is_none());
        assert!(config.server.https_socket_addr().is_none());
        assert_eq!(config.server.http_port, 8080);
    }

    #[test]
    fn empty_config_falls_back_to_defaults() {
        let config = load_yaml("").unwrap();
        assert!(config.server.tls_config.is_none());
        assert_eq!(
            config.server.http_socket_addr(),
            "127.0.0.1:3000".parse().unwrap()
        );
    }

    #[test]
    fn tls_block_parses() {
        let config = load_yaml(
            "server:\n\
             \x20 host: 0.0.0.0\n\
             \x20 http_port: 80\n\
             \x20 tls_config:\n\
             \x20   https_port: 443\n\
             \x20   lets_encrypt:\n\
             \x20     domains:\n\
             \x20       - slash0.dev\n\
             \x20       - www.slash0.dev\n\
             \x20     certs_dir: /work/certs\n\
             \x20     prod_letsencrypt: true\n",
        )
        .unwrap();

        let tls = config.server.tls_config.as_ref().unwrap();
        assert_eq!(tls.https_port, 443);
        assert_eq!(tls.lets_encrypt.domains, ["slash0.dev", "www.slash0.dev"]);
        assert_eq!(tls.lets_encrypt.certs_dir, PathBuf::from("/work/certs"));
        assert!(tls.lets_encrypt.prod_letsencrypt);
        assert_eq!(
            config.server.https_socket_addr(),
            Some("0.0.0.0:443".parse().unwrap())
        );
    }

    #[test]
    fn scalar_domain_is_rejected() {
        // `domains: slash0.dev` reads naturally but is a string, not a list.
        load_yaml(
            "server:\n\
             \x20 tls_config:\n\
             \x20   https_port: 443\n\
             \x20   lets_encrypt:\n\
             \x20     domains: slash0.dev\n\
             \x20     certs_dir: ./certs\n\
             \x20     prod_letsencrypt: false\n",
        )
        .unwrap_err();
    }

    #[test]
    fn tls_block_without_lets_encrypt_is_rejected() {
        load_yaml("server:\n  tls_config:\n    https_port: 443\n").unwrap_err();
    }

    /// Both files are copied verbatim onto a host, so a typo in either only
    /// shows up at deploy time otherwise.
    #[test]
    fn shipped_example_configs_parse() {
        let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();

        for example in ["config/server.example.yaml", "deploy/server.yaml.example"] {
            extract(Yaml::file(workspace_root.join(example)))
                .unwrap_or_else(|err| panic!("{example} failed to parse: {err}"));
        }
    }

    #[test]
    fn logging_defaults_when_block_is_absent() {
        let logging = load_yaml("").unwrap().logging;
        assert_eq!(logging.format, LogFormat::Compact);
        assert!(logging.ansi);
        assert!(logging.file.is_none());
    }

    #[test]
    fn file_logging_needs_only_a_dir() {
        let file = load_yaml("logging:\n  file:\n    dir: /var/log/slash0\n")
            .unwrap()
            .logging
            .file
            .expect("file logging is configured");

        assert_eq!(file.dir, PathBuf::from("/var/log/slash0"));
        assert_eq!(file.prefix, "slash0");
        assert_eq!(file.max_file_size_mb, 64);
        assert_eq!(file.max_files, 14);
        assert_eq!(file.keep_uncompressed, Some(0));
    }

    #[test]
    fn file_block_without_dir_is_rejected() {
        load_yaml("logging:\n  file:\n    prefix: slash0\n").unwrap_err();
    }

    #[test]
    fn zero_max_file_size_is_rejected() {
        // `FileRotate` panics on a zero-byte limit, so this has to fail here.
        load_yaml("logging:\n  file:\n    dir: /var/log/slash0\n    max_file_size_mb: 0\n")
            .unwrap_err();
    }

    #[test]
    fn keep_uncompressed_round_trips_as_count_and_as_none() {
        let count =
            load_yaml("logging:\n  file:\n    dir: /var/log/slash0\n    keep_uncompressed: 3\n")
                .unwrap();
        assert_eq!(count.logging.file.unwrap().keep_uncompressed, Some(3));

        let disabled =
            load_yaml("logging:\n  file:\n    dir: /var/log/slash0\n    keep_uncompressed: null\n")
                .unwrap();
        assert_eq!(disabled.logging.file.unwrap().keep_uncompressed, None);
    }

    #[test]
    fn log_format_parses_and_rejects_unknown_values() {
        assert_eq!(
            load_yaml("logging:\n  format: pretty\n")
                .unwrap()
                .logging
                .format,
            LogFormat::Pretty
        );
        load_yaml("logging:\n  format: json\n").unwrap_err();
    }

    #[test]
    fn lets_encrypt_block_missing_domains_is_rejected() {
        load_yaml(
            "server:\n\
             \x20 tls_config:\n\
             \x20   https_port: 443\n\
             \x20   lets_encrypt:\n\
             \x20     certs_dir: ./certs\n\
             \x20     prod_letsencrypt: false\n",
        )
        .unwrap_err();
    }
}
