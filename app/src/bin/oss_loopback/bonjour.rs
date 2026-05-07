use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use axum::{extract::State, Json};
use serde::Serialize;
use tokio::sync::RwLock;

use super::{
    state::{local_hostname, sanitize_identifier, LocalAccount, LocalAgentWorkerConfig},
    ServerState,
};

const AGENT_SERVICE_TYPE: &str = "_warpsolo-agent._tcp.local.";

pub(crate) type DiscoveredWorkerStore = Arc<RwLock<HashMap<String, DiscoveredWorker>>>;

pub(crate) fn new_discovered_worker_store() -> DiscoveredWorkerStore {
    Arc::new(RwLock::new(HashMap::new()))
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredWorker {
    service_name: String,
    device_id: String,
    user_id: String,
    display_name: String,
    hostname: String,
    url: String,
    capabilities: Vec<String>,
    auth: String,
    last_seen_epoch_millis: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiscoveredWorkersResponse {
    workers: Vec<DiscoveredWorker>,
}

pub(crate) struct BonjourRuntime {
    daemon: mdns_sd::ServiceDaemon,
    _browse_thread: std::thread::JoinHandle<()>,
}

impl Drop for BonjourRuntime {
    fn drop(&mut self) {
        if let Err(err) = self.daemon.shutdown() {
            log::debug!("Failed to shut down WarpSOLO Bonjour daemon: {err}");
        }
    }
}

pub(crate) fn start(state: ServerState, worker_addr: Option<SocketAddr>) -> Result<BonjourRuntime> {
    let daemon =
        mdns_sd::ServiceDaemon::new().context("failed to start WarpSOLO Bonjour daemon")?;
    let receiver = daemon
        .browse(AGENT_SERVICE_TYPE)
        .context("failed to browse for WarpSOLO agent workers")?;

    if let Some(worker_addr) = worker_addr {
        if let Err(err) = publish_worker_service(&daemon, &state, worker_addr) {
            log::warn!("Failed to publish WarpSOLO agent worker over Bonjour: {err:#}");
        }
    }

    let account = state.account.clone();
    let store = state.discovered_workers.clone();
    let browse_thread = std::thread::Builder::new()
        .name("warpsolo-bonjour-browse".to_string())
        .spawn(move || {
            while let Ok(event) = receiver.recv() {
                handle_service_event(&account, &store, event);
            }
        })
        .context("failed to spawn WarpSOLO Bonjour browse thread")?;

    log::info!("Started WarpSOLO Bonjour discovery for {AGENT_SERVICE_TYPE}");
    Ok(BonjourRuntime {
        daemon,
        _browse_thread: browse_thread,
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

impl DiscoveredWorker {
    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    fn matches(&self, worker_host: &str) -> bool {
        [self.device_id(), self.display_name(), self.hostname()]
            .into_iter()
            .any(|candidate| candidate.eq_ignore_ascii_case(worker_host))
            || self.service_name.eq_ignore_ascii_case(worker_host)
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

fn publish_worker_service(
    daemon: &mdns_sd::ServiceDaemon,
    state: &ServerState,
    worker_addr: SocketAddr,
) -> Result<()> {
    let worker_ip = worker_addr.ip();
    if worker_ip.is_loopback() {
        log::warn!(
            "WarpSOLO agent worker is bound to {worker_addr}; not advertising loopback over Bonjour"
        );
        return Ok(());
    }

    let properties = vec![
        ("version".to_string(), "1".to_string()),
        ("device_id".to_string(), state.account.device_id.clone()),
        ("user_id".to_string(), state.account.user_id.clone()),
        ("hostname".to_string(), local_hostname()),
        (
            "display_name".to_string(),
            state.account.display_name.clone(),
        ),
        ("app".to_string(), "WarpSOLO".to_string()),
        ("api".to_string(), "http".to_string()),
        (
            "capabilities".to_string(),
            "agent,terminal,workspace".to_string(),
        ),
        (
            "auth".to_string(),
            worker_auth(&state.worker_config).to_string(),
        ),
    ];
    let instance_name = state.account.device_id.clone();
    let host_name = bonjour_host_name();
    let service_info = if worker_ip.is_unspecified() {
        mdns_sd::ServiceInfo::new(
            AGENT_SERVICE_TYPE,
            &instance_name,
            &host_name,
            (),
            worker_addr.port(),
            &properties[..],
        )?
        .enable_addr_auto()
    } else {
        mdns_sd::ServiceInfo::new(
            AGENT_SERVICE_TYPE,
            &instance_name,
            &host_name,
            worker_ip,
            worker_addr.port(),
            &properties[..],
        )?
    };

    daemon
        .register(service_info)
        .context("failed to register WarpSOLO agent worker service")?;
    log::info!("Published WarpSOLO agent worker over Bonjour at {worker_addr}");
    Ok(())
}

fn handle_service_event(
    account: &LocalAccount,
    store: &DiscoveredWorkerStore,
    event: mdns_sd::ServiceEvent,
) {
    match event {
        mdns_sd::ServiceEvent::ServiceResolved(service) => {
            let Some(worker) = discovered_worker_from_service(account, &service) else {
                return;
            };
            let key = worker.device_id.clone();
            log::debug!(
                "Discovered WarpSOLO agent worker {} at {}",
                worker.display_name,
                worker.url
            );
            store.blocking_write().insert(key, worker);
        }
        mdns_sd::ServiceEvent::ServiceRemoved(_, service_name) => {
            store
                .blocking_write()
                .retain(|_, worker| worker.service_name != service_name);
            log::debug!("Removed WarpSOLO agent worker service {service_name}");
        }
        mdns_sd::ServiceEvent::SearchStarted(service_type) => {
            log::debug!("Started Bonjour search for {service_type}");
        }
        mdns_sd::ServiceEvent::SearchStopped(service_type) => {
            log::debug!("Stopped Bonjour search for {service_type}");
        }
        mdns_sd::ServiceEvent::ServiceFound(_, service_name) => {
            log::debug!("Found WarpSOLO agent worker service {service_name}");
        }
        _ => {}
    }
}

fn discovered_worker_from_service(
    account: &LocalAccount,
    service: &mdns_sd::ResolvedService,
) -> Option<DiscoveredWorker> {
    let device_id = service
        .get_property_val_str("device_id")
        .map(str::to_owned)
        .unwrap_or_else(|| service.get_fullname().to_string());
    if device_id == account.device_id {
        return None;
    }

    let url = worker_url_from_service(service)?;
    let capabilities = service
        .get_property_val_str("capabilities")
        .map(parse_capabilities)
        .filter(|capabilities| !capabilities.is_empty())
        .unwrap_or_else(|| {
            vec![
                "agent".to_string(),
                "terminal".to_string(),
                "workspace".to_string(),
            ]
        });

    Some(DiscoveredWorker {
        service_name: service.get_fullname().to_string(),
        device_id,
        user_id: service
            .get_property_val_str("user_id")
            .unwrap_or("unknown")
            .to_string(),
        display_name: service
            .get_property_val_str("display_name")
            .unwrap_or_else(|| service.get_fullname())
            .to_string(),
        hostname: service.get_hostname().to_string(),
        url,
        capabilities,
        auth: service
            .get_property_val_str("auth")
            .unwrap_or("none")
            .to_string(),
        last_seen_epoch_millis: now_epoch_millis(),
    })
}

fn worker_url_from_service(service: &mdns_sd::ResolvedService) -> Option<String> {
    let mut addresses = service
        .get_addresses()
        .iter()
        .map(|address| address.to_ip_addr())
        .filter(|address| !address.is_unspecified())
        .collect::<Vec<_>>();
    addresses.sort_by_key(|address| {
        (
            address.is_loopback(),
            !matches!(address, IpAddr::V4(_)),
            address.to_string(),
        )
    });

    addresses.into_iter().next().map(|address| match address {
        IpAddr::V4(address) => format!("http://{address}:{}", service.get_port()),
        IpAddr::V6(address) => format!("http://[{address}]:{}", service.get_port()),
    })
}

fn parse_capabilities(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|capability| !capability.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn now_epoch_millis() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    now.as_millis().try_into().unwrap_or(u64::MAX)
}

fn bonjour_host_name() -> String {
    let host = sanitize_identifier(&local_hostname());
    if host.ends_with(".local") {
        format!("{host}.")
    } else {
        format!("{host}.local.")
    }
}
