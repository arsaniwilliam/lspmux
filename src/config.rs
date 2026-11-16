use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::net::{IpAddr, Ipv4Addr};
#[cfg(target_family = "unix")]
use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::de::{Error, Unexpected};
use serde::{Deserialize, Deserializer, Serialize};

mod default {
    use super::*;

    pub fn instance_timeout() -> Option<u32> {
        // 5 minutes
        Some(5 * 60)
    }

    pub fn gc_interval() -> u32 {
        // 10 seconds
        10
    }

    pub fn listen() -> Address {
        // localhost & some random unprivileged port
        Address::Tcp(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 27_631)
    }

    pub fn connect() -> Address {
        listen()
    }

    pub fn log_filters() -> String {
        "info".to_owned()
    }

    pub fn pass_environment() -> BTreeSet<String> {
        BTreeSet::new()
    }

    pub fn settle_ms() -> u64 {
        1500
    }

    pub fn memory_poll_ms() -> u64 {
        2000
    }
}

mod de {
    use super::*;

    /// parse either bool(false) or u32
    pub fn instance_timeout<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOf {
            Bool(bool),
            U32(u32),
        }

        match OneOf::deserialize(deserializer) {
            Ok(OneOf::U32(value)) => Ok(Some(value)),
            Ok(OneOf::Bool(false)) => Ok(None),
            Ok(OneOf::Bool(true)) => Err(Error::invalid_value(
                Unexpected::Bool(true),
                &"a non-negative integer or false",
            )),
            Err(_) => Err(Error::custom(
                "invalid type: expected a non-negative integer or false",
            )),
        }
    }

    /// make sure the value is greater than 0, a timer can't tick every 0ms
    pub fn memory_poll_ms<'de, D>(deserializer: D) -> Result<u64, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u64::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }

    /// make sure the value is greater than 0 to giver users feedback on invalid configuration
    pub fn gc_interval<'de, D>(deserializer: D) -> Result<u32, D::Error>
    where
        D: Deserializer<'de>,
    {
        match u32::deserialize(deserializer)? {
            0 => Err(Error::invalid_value(
                Unexpected::Unsigned(0),
                &"an integer 1 or greater",
            )),
            value => Ok(value),
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(untagged)]
pub enum Address {
    Tcp(IpAddr, u16),
    #[cfg(target_family = "unix")]
    Unix(PathBuf),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default::instance_timeout")]
    #[serde(deserialize_with = "de::instance_timeout")]
    pub instance_timeout: Option<u32>,

    #[serde(default = "default::gc_interval")]
    #[serde(deserialize_with = "de::gc_interval")]
    pub gc_interval: u32,

    #[serde(default = "default::listen")]
    pub listen: Address,

    #[serde(default = "default::connect")]
    pub connect: Address,

    #[serde(default = "default::log_filters")]
    pub log_filters: String,

    #[serde(default = "default::pass_environment")]
    pub pass_environment: BTreeSet<String>,

    /// Milliseconds the server must be quiet before lspmux answers owed
    /// diagnostics itself
    #[serde(default = "default::settle_ms")]
    pub settle_ms: u64,

    /// Milliseconds between reads of `/proc/<pid>/status` of servers with a
    /// `recycle` entry
    #[serde(default = "default::memory_poll_ms")]
    #[serde(deserialize_with = "de::memory_poll_ms")]
    pub memory_poll_ms: u64,

    /// `--server-path` value to the JSON file with the settings lspmux
    /// answers `workspace/configuration` with
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub server_settings: BTreeMap<String, String>,

    /// `--server-path` value to the memory limits at which lspmux replaces
    /// the server process behind its connected clients
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub recycle: BTreeMap<String, RecycleConfig>,
}

/// When to replace a language server process, all in MB
///
/// Without a limit the corresponding trigger doesn't exist.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RecycleConfig {
    /// Latest logged live heap bytes at which the server is replaced once it
    /// settled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub soft_live_mb: Option<u64>,

    /// Live heap bytes at which the server is replaced immediately
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forced_live_mb: Option<u64>,

    /// `VmRSS + VmSwap` of the server process at which it is replaced
    /// immediately
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_memory_mb: Option<u64>,
}

#[cfg(test)]
#[test]
fn generate_default_and_check_it_matches_commited_defaults() {
    use std::fs;
    use std::path::Path;

    let generated_defaults = Config::default();
    let generated_defaults = toml::to_string(&generated_defaults).expect("failed serialize");

    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("defaults.toml");
    let saved_defaults = fs::read_to_string(path).expect("failed reading defaults.toml file");

    assert_eq!(generated_defaults, saved_defaults);
}

impl Default for Config {
    fn default() -> Self {
        Config {
            instance_timeout: default::instance_timeout(),
            gc_interval: default::gc_interval(),
            listen: default::listen(),
            connect: default::connect(),
            log_filters: default::log_filters(),
            pass_environment: default::pass_environment(),
            settle_ms: default::settle_ms(),
            memory_poll_ms: default::memory_poll_ms(),
            server_settings: BTreeMap::new(),
            recycle: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Try loading config file from the system default location
    pub fn try_load() -> Result<Self> {
        let pkg_name = env!("CARGO_PKG_NAME");
        let config_path = ProjectDirs::from("", "", pkg_name)
            .context("project config directory not found")?
            .config_dir()
            .join("config.toml");
        let path = config_path.display();
        let config_data =
            fs::read(&config_path).with_context(|| format!("cannot read config file `{path}`"))?;
        toml::from_slice(&config_data).with_context(|| format!("cannot parse config file `{path}`"))
    }

    /// Configure tracing-subscriber with env filter set to `log_filters` (if
    /// not overriden by RUST_LOG env var)
    ///
    /// Panics if called multiple times.
    pub fn init_logger(&self) {
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::EnvFilter;

        let format = tracing_subscriber::fmt::layer()
            .without_time()
            .with_target(false)
            .with_writer(std::io::stderr);

        let filter = EnvFilter::try_from_default_env()
            .or_else(|_| EnvFilter::try_new(&self.log_filters))
            .unwrap_or_else(|_| EnvFilter::new("info"));

        tracing_subscriber::registry()
            .with(filter)
            .with(format)
            .init();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recycle_tables_per_server_path() {
        let config: Config = toml::from_str(
            r#"
            memory_poll_ms = 500

            [recycle."haskell-language-server-wrapper"]
            soft_live_mb = 12288
            forced_live_mb = 24576
            max_memory_mb = 32768

            [recycle.partial]
            soft_live_mb = 1
            "#,
        )
        .unwrap();
        assert_eq!(config.memory_poll_ms, 500);
        assert_eq!(
            config.recycle["haskell-language-server-wrapper"],
            RecycleConfig {
                soft_live_mb: Some(12288),
                forced_live_mb: Some(24576),
                max_memory_mb: Some(32768),
            }
        );
        assert_eq!(config.recycle["partial"].forced_live_mb, None);
        assert!(!config.recycle.contains_key("rust-analyzer"));

        // survives serialization
        let again: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(again.recycle, config.recycle);
    }

    #[test]
    fn recycle_rejects_unknown_keys_and_zero_poll() {
        assert!(toml::from_str::<Config>("[recycle.x]\nsoft_mb = 1").is_err());
        assert!(toml::from_str::<Config>("memory_poll_ms = 0").is_err());
    }
}
