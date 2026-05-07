use std::{fs, net::SocketAddr, str::FromStr};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

const LOCAL_ACCOUNT_FILE: &str = "local-account.json";
const LOCAL_AGENT_WORKER_FILE: &str = "agent-worker.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct LocalAccount {
    pub(crate) user_id: String,
    pub(crate) device_id: String,
    pub(crate) display_name: String,
    pub(crate) id_token: String,
    pub(crate) refresh_token: String,
    pub(crate) custom_token: String,
}

impl LocalAccount {
    pub(crate) fn load_or_create() -> Result<Self> {
        let path = account_path();
        if path.exists() {
            let mut account: Self = fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))
                .and_then(|contents| {
                    serde_json::from_str(&contents)
                        .with_context(|| format!("failed to parse {}", path.display()))
                })?;
            if account.sync_local_identity() {
                let contents = serde_json::to_string_pretty(&account)?;
                fs::write(&path, contents)
                    .with_context(|| format!("failed to write {}", path.display()))?;
            }
            return Ok(account);
        }

        let account = Self {
            user_id: local_user_id(),
            device_id: local_device_id(),
            display_name: local_display_name(),
            id_token: format!("local-id-token-{}", Uuid::new_v4()),
            refresh_token: format!("local-refresh-token-{}", Uuid::new_v4()),
            custom_token: format!("local-custom-token-{}", Uuid::new_v4()),
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let contents = serde_json::to_string_pretty(&account)?;
        fs::write(&path, contents)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(account)
    }

    fn sync_local_identity(&mut self) -> bool {
        let user_id = local_user_id();
        let device_id = local_device_id();
        let display_name = local_display_name();
        let changed = self.user_id != user_id
            || self.device_id != device_id
            || self.display_name != display_name;
        if changed {
            self.user_id = user_id;
            self.device_id = device_id;
            self.display_name = display_name;
        }
        changed
    }
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct LocalAgentWorkerConfig {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default = "default_agent_worker_bind")]
    bind: String,
    #[serde(default)]
    port: u16,
    pub(crate) pairing_token: Option<String>,
    #[serde(default)]
    pub(crate) peers: Vec<LocalAgentPeerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
pub(crate) struct LocalAgentPeerConfig {
    pub(crate) name: Option<String>,
    pub(crate) device_id: Option<String>,
    pub(crate) user_id: Option<String>,
    pub(crate) hostname: Option<String>,
    pub(crate) url: String,
}

impl Default for LocalAgentWorkerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_agent_worker_bind(),
            port: 0,
            pairing_token: None,
            peers: Vec::new(),
        }
    }
}

impl LocalAgentWorkerConfig {
    pub(crate) fn load() -> Result<Self> {
        let path = agent_worker_config_path();
        if !path.exists() {
            return Ok(Self::default());
        }

        fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))
            .and_then(|contents| {
                toml::from_str(&contents)
                    .with_context(|| format!("failed to parse {}", path.display()))
            })
    }

    pub(crate) fn bind_addr(&self) -> Result<SocketAddr> {
        let bind = self.bind.trim();
        let bind = if bind.is_empty() {
            default_agent_worker_bind()
        } else {
            bind.to_owned()
        };
        SocketAddr::from_str(&format!("{bind}:{}", self.port))
            .with_context(|| format!("invalid agent worker bind address: {bind}:{}", self.port))
    }

    pub(crate) const fn port(&self) -> u16 {
        self.port
    }
}

pub(crate) fn local_hostname() -> String {
    gethostname::gethostname()
        .into_string()
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

pub(crate) fn sanitize_identifier(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn account_path() -> std::path::PathBuf {
    warp_core::paths::config_local_dir().join(LOCAL_ACCOUNT_FILE)
}

fn agent_worker_config_path() -> std::path::PathBuf {
    warp_core::paths::config_local_dir().join(LOCAL_AGENT_WORKER_FILE)
}

fn local_user_id() -> String {
    format!("local-user-{}", sanitize_identifier(&local_username()))
}

fn local_device_id() -> String {
    format!("local-device-{}", sanitize_identifier(&local_hostname()))
}

fn local_display_name() -> String {
    format!("{}@{}", local_username(), local_hostname())
}

fn local_username() -> String {
    ["USER", "LOGNAME", "USERNAME"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

fn default_agent_worker_bind() -> String {
    "127.0.0.1".to_string()
}
