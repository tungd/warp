use std::{
    collections::HashMap,
    hash::Hasher,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{extract::State, Json};
use serde::Serialize;
use tokio::sync::RwLock;

use super::{
    state::{LocalAccount, LocalAgentWorkerConfig},
    ServerState,
};

const SYNTHETIC_ENVIRONMENT_ID_PREFIX: &str = "wsolo-";

pub(crate) type DiscoveredWorkerStore = Arc<RwLock<HashMap<String, DiscoveredWorker>>>;

pub(crate) fn new_discovered_worker_store() -> DiscoveredWorkerStore {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredWorker {
    pub(crate) source_id: String,
    pub(crate) device_id: String,
    pub(crate) user_id: String,
    pub(crate) display_name: String,
    pub(crate) hostname: String,
    pub(crate) url: String,
    pub(crate) capabilities: Vec<String>,
    pub(crate) auth: String,
    pub(crate) last_seen_epoch_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredWorkersResponse {
    workers: Vec<DiscoveredWorker>,
}

pub(crate) fn seed_static_workers(state: &ServerState) {
    let workers = state
        .worker_config
        .peers
        .iter()
        .filter_map(|peer| static_worker_from_config(&state.account, peer))
        .collect::<Vec<_>>();
    if workers.is_empty() {
        return;
    }

    let mut store = state.discovered_workers.blocking_write();
    for worker in workers {
        log::info!(
            "Configured static WarpSOLO agent worker {} at {}",
            worker.display_name,
            worker.url
        );
        store.insert(worker.device_id.clone(), worker);
    }
}

fn static_worker_from_config(
    account: &LocalAccount,
    peer: &super::state::LocalAgentPeerConfig,
) -> Option<DiscoveredWorker> {
    let url = peer.url.trim().trim_end_matches('/').to_string();
    if url.is_empty() {
        return None;
    }

    let device_id = peer
        .device_id
        .as_deref()
        .map(str::trim)
        .filter(|device_id| !device_id.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("static-peer-{:016x}", stable_hash64(&url)));
    if device_id == account.device_id {
        return None;
    }

    let display_name = peer
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&device_id)
        .to_string();
    let hostname = peer
        .hostname
        .as_deref()
        .map(str::trim)
        .filter(|hostname| !hostname.is_empty())
        .unwrap_or(&device_id)
        .to_string();
    let user_id = peer
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|user_id| !user_id.is_empty())
        .unwrap_or("static-peer")
        .to_string();

    Some(DiscoveredWorker {
        source_id: format!("static:{device_id}"),
        device_id,
        user_id,
        display_name,
        hostname,
        url,
        capabilities: default_capabilities(),
        auth: "none".to_string(),
        last_seen_epoch_millis: now_epoch_millis(),
    })
}

pub(crate) async fn discovered_workers(
    State(state): State<ServerState>,
) -> Json<DiscoveredWorkersResponse> {
    let mut workers = state
        .discovered_workers
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    workers.sort_by(|left, right| {
        left.display_name
            .cmp(&right.display_name)
            .then_with(|| left.device_id.cmp(&right.device_id))
    });

    Json(DiscoveredWorkersResponse { workers })
}

pub(crate) async fn find_worker(
    store: &DiscoveredWorkerStore,
    worker_host: &str,
) -> Option<DiscoveredWorker> {
    let worker_host = worker_host.trim();
    let workers = store.read().await;

    if worker_host.is_empty() || worker_host.eq_ignore_ascii_case("warp") {
        return (workers.len() == 1)
            .then(|| workers.values().next().cloned())
            .flatten();
    }

    workers
        .values()
        .find(|worker| worker.matches(worker_host))
        .cloned()
}

pub(crate) async fn find_worker_by_synthetic_environment_id(
    store: &DiscoveredWorkerStore,
    environment_id: &str,
) -> Option<DiscoveredWorker> {
    let environment_id = environment_id.trim();
    if !is_synthetic_environment_id(environment_id) {
        return None;
    }

    store
        .read()
        .await
        .values()
        .find(|worker| synthetic_environment_id_for_device_id(&worker.device_id) == environment_id)
        .cloned()
}

pub(crate) fn synthetic_environment_id_for_worker(worker: &DiscoveredWorker) -> String {
    synthetic_environment_id_for_device_id(&worker.device_id)
}

pub(crate) fn is_synthetic_environment_id(environment_id: &str) -> bool {
    environment_id.starts_with(SYNTHETIC_ENVIRONMENT_ID_PREFIX) && environment_id.len() == 22
}

pub(crate) fn default_capabilities() -> Vec<String> {
    vec![
        "agent".to_string(),
        "terminal".to_string(),
        "workspace".to_string(),
    ]
}

pub(crate) fn worker_auth(worker_config: &LocalAgentWorkerConfig) -> &'static str {
    if worker_config
        .pairing_token
        .as_deref()
        .is_some_and(|token| !token.trim().is_empty())
    {
        "pairing-token-v1"
    } else {
        "none"
    }
}

pub(crate) fn now_epoch_millis() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis().try_into().unwrap_or(u64::MAX)
}

fn synthetic_environment_id_for_device_id(device_id: &str) -> String {
    format!(
        "{SYNTHETIC_ENVIRONMENT_ID_PREFIX}{:016x}",
        stable_hash64(device_id)
    )
}

fn stable_hash64(value: &str) -> u64 {
    let mut hasher = Fnv1a64::default();
    hasher.write(value.as_bytes());
    hasher.finish()
}

#[derive(Default)]
struct Fnv1a64(u64);

impl Hasher for Fnv1a64 {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        if self.0 == 0 {
            self.0 = 0xcbf29ce484222325;
        }
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }
}

impl DiscoveredWorker {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    fn matches(&self, worker_host: &str) -> bool {
        [self.device_id(), self.display_name(), self.hostname()]
            .into_iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(worker_host))
            || self.source_id.eq_ignore_ascii_case(worker_host)
            || synthetic_environment_id_for_worker(self).eq_ignore_ascii_case(worker_host)
    }

    fn device_id(&self) -> &str {
        &self.device_id
    }

    fn display_name(&self) -> &str {
        &self.display_name
    }

    fn hostname(&self) -> &str {
        &self.hostname
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::LocalAgentPeerConfig;
    use super::*;

    #[test]
    fn static_worker_uses_configured_peer_identity() {
        let account = LocalAccount {
            user_id: "local-user-me".to_string(),
            device_id: "local-device-me.local".to_string(),
            display_name: "me@host.local".to_string(),
            id_token: "id-token".to_string(),
            refresh_token: "refresh-token".to_string(),
            custom_token: "custom-token".to_string(),
        };
        let peer = LocalAgentPeerConfig {
            name: Some("td@mars.local".to_string()),
            device_id: Some("local-device-mars.local".to_string()),
            user_id: Some("local-user-td".to_string()),
            hostname: Some("mars.local".to_string()),
            url: "http://100.84.248.34:9109/".to_string(),
        };

        let worker = static_worker_from_config(&account, &peer).expect("static worker");

        assert_eq!(worker.device_id, "local-device-mars.local");
        assert_eq!(worker.source_id, "static:local-device-mars.local");
        assert_eq!(worker.display_name, "td@mars.local");
        assert_eq!(worker.hostname, "mars.local");
        assert_eq!(worker.url, "http://100.84.248.34:9109");
    }
}
