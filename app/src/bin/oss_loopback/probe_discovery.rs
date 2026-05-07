use std::{
    collections::HashSet,
    net::{IpAddr, Ipv4Addr},
    time::Duration,
};

use anyhow::{Context, Result};
use futures_util::{stream, StreamExt};
use serde::Deserialize;
use tokio::{process::Command, runtime::Runtime};

use super::{worker_discovery, ServerState};

const DISCOVERY_INTERVAL: Duration = Duration::from_secs(20);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_CONCURRENCY: usize = 64;
const MAX_LOCAL_SUBNET_HOSTS: u32 = 254;

pub(crate) fn start(state: ServerState, runtime: &Runtime) {
    let port = state.worker_config.port();
    if port == 0 {
        log::debug!("WarpSOLO probe discovery is disabled because worker port is 0");
        return;
    }

    runtime.spawn(async move {
        loop {
            discover_once(&state, port).await;
            tokio::time::sleep(DISCOVERY_INTERVAL).await;
        }
    });
}

pub(crate) async fn refresh(state: &ServerState) {
    let port = state.worker_config.port();
    if port == 0 {
        return;
    }
    discover_once(state, port).await;
}

async fn discover_once(state: &ServerState, port: u16) {
    let candidates = probe_candidates(port).await;
    let mut current_probe_source_ids = HashSet::new();

    let mut probes = stream::iter(
        candidates
            .into_iter()
            .map(|candidate| probe_worker(state, candidate)),
    )
    .buffer_unordered(PROBE_CONCURRENCY);

    while let Some(worker) = probes.next().await {
        let Some(worker) = worker else {
            continue;
        };
        if worker.device_id == state.account.device_id {
            continue;
        }

        current_probe_source_ids.insert(worker.source_id.clone());
        log::debug!(
            "Discovered WarpSOLO worker {} at {}",
            worker.display_name,
            worker.url
        );
        state
            .discovered_workers
            .write()
            .await
            .insert(worker.device_id.clone(), worker);
    }

    state.discovered_workers.write().await.retain(|_, worker| {
        !worker.source_id.starts_with("probe:")
            || current_probe_source_ids.contains(&worker.source_id)
    });
}

async fn probe_candidates(port: u16) -> Vec<ProbeCandidate> {
    let mut candidates = Vec::new();
    match local_subnet_candidates(port) {
        Ok(local_candidates) => candidates.extend(local_candidates),
        Err(err) => log::debug!("WarpSOLO local subnet discovery is unavailable: {err:#}"),
    }

    match tailscale_candidates(port).await {
        Ok(tailscale_candidates) => candidates.extend(tailscale_candidates),
        Err(err) => log::debug!("WarpSOLO Tailscale candidate discovery is unavailable: {err:#}"),
    }

    dedupe_candidates(candidates)
}

fn dedupe_candidates(candidates: Vec<ProbeCandidate>) -> Vec<ProbeCandidate> {
    let mut seen_urls = HashSet::new();
    candidates
        .into_iter()
        .filter(|candidate| seen_urls.insert(candidate.url.clone()))
        .collect()
}

async fn probe_worker(
    state: &ServerState,
    candidate: ProbeCandidate,
) -> Option<worker_discovery::DiscoveredWorker> {
    let health_url = format!("{}/worker/health", candidate.url);
    let health: WorkerHealth = state
        .client
        .get(&health_url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    if !health.ok || !health.app.eq_ignore_ascii_case("WarpSOLO") {
        return None;
    }

    let capabilities_url = format!("{}/worker/capabilities", candidate.url);
    let capabilities: Option<WorkerCapabilities> = match state
        .client
        .get(&capabilities_url)
        .timeout(PROBE_TIMEOUT)
        .send()
        .await
    {
        Ok(response) => response.error_for_status().ok()?.json().await.ok(),
        Err(_) => None,
    };

    Some(worker_discovery::DiscoveredWorker {
        source_id: candidate.source_id,
        device_id: health.device_id,
        user_id: health.user_id,
        display_name: health.display_name,
        hostname: candidate.hostname,
        url: candidate.url,
        capabilities: capabilities
            .as_ref()
            .and_then(|capabilities| capabilities.capabilities.clone())
            .filter(|capabilities| !capabilities.is_empty())
            .unwrap_or_else(worker_discovery::default_capabilities),
        auth: capabilities
            .and_then(|capabilities| capabilities.auth)
            .unwrap_or_else(|| "none".to_string()),
        last_seen_epoch_millis: worker_discovery::now_epoch_millis(),
    })
}

fn local_subnet_candidates(port: u16) -> Result<Vec<ProbeCandidate>> {
    Ok(local_ipv4_subnets()?
        .into_iter()
        .flat_map(|subnet| probe_candidates_for_ipv4_subnet(subnet, port))
        .collect::<Vec<_>>())
}

fn probe_candidates_for_ipv4_subnet(subnet: LocalIpv4Subnet, port: u16) -> Vec<ProbeCandidate> {
    let mask = if subnet.host_count() > MAX_LOCAL_SUBNET_HOSTS {
        u32::MAX << 8
    } else {
        subnet.mask
    };
    let network = subnet.addr_u32 & mask;
    let broadcast = network | !mask;
    if broadcast <= network + 1 {
        return Vec::new();
    }

    ((network + 1)..broadcast)
        .filter(|addr| *addr != subnet.addr_u32)
        .map(|addr| {
            let address = Ipv4Addr::from(addr);
            ProbeCandidate {
                source_id: format!("probe:local:{address}"),
                hostname: address.to_string(),
                url: format!("http://{address}:{port}"),
            }
        })
        .collect()
}

fn local_ipv4_subnets() -> Result<Vec<LocalIpv4Subnet>> {
    let mut interfaces = std::ptr::null_mut();
    unsafe {
        if libc::getifaddrs(&mut interfaces) != 0 {
            return Err(std::io::Error::last_os_error()).context("getifaddrs failed");
        }
    }

    let mut subnets = Vec::new();
    let mut cursor = interfaces;
    while !cursor.is_null() {
        let interface = unsafe { &*cursor };
        if let (Some(addr), Some(mask)) = unsafe {
            (
                sockaddr_ipv4(interface.ifa_addr),
                sockaddr_ipv4(interface.ifa_netmask),
            )
        } {
            if should_scan_ipv4(addr) {
                let mask_u32 = u32::from(mask);
                if is_contiguous_ipv4_mask(mask_u32) {
                    subnets.push(LocalIpv4Subnet {
                        addr,
                        addr_u32: u32::from(addr),
                        mask: mask_u32,
                    });
                }
            }
        }
        cursor = interface.ifa_next;
    }

    unsafe {
        libc::freeifaddrs(interfaces);
    }

    Ok(dedupe_subnets(subnets))
}

unsafe fn sockaddr_ipv4(addr: *const libc::sockaddr) -> Option<Ipv4Addr> {
    if addr.is_null() || unsafe { (*addr).sa_family as i32 } != libc::AF_INET {
        return None;
    }

    let addr = unsafe { *(addr as *const libc::sockaddr_in) };
    Some(Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr)))
}

fn should_scan_ipv4(addr: Ipv4Addr) -> bool {
    !addr.is_loopback()
        && !addr.is_link_local()
        && !addr.is_broadcast()
        && !addr.is_unspecified()
        && !addr.is_multicast()
}

fn is_contiguous_ipv4_mask(mask: u32) -> bool {
    let prefix = mask.leading_ones();
    let expected = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    mask == expected
}

fn dedupe_subnets(subnets: Vec<LocalIpv4Subnet>) -> Vec<LocalIpv4Subnet> {
    let mut seen = HashSet::new();
    subnets
        .into_iter()
        .filter(|subnet| seen.insert((subnet.addr, subnet.mask)))
        .collect()
}

async fn tailscale_candidates(port: u16) -> Result<Vec<ProbeCandidate>> {
    let status = tailscale_status().await?;
    Ok(tailscale_peer_candidates_from_status(&status, port))
}

async fn tailscale_status() -> Result<TailscaleStatus> {
    let output = Command::new("tailscale")
        .arg("status")
        .arg("--json")
        .output()
        .await
        .context("failed to run tailscale status --json")?;
    if !output.status.success() {
        anyhow::bail!(
            "tailscale status --json exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    serde_json::from_slice(&output.stdout).context("failed to parse tailscale status --json")
}

fn tailscale_peer_candidates_from_status(
    status: &TailscaleStatus,
    port: u16,
) -> Vec<ProbeCandidate> {
    status
        .peer
        .values()
        .filter(|peer| peer.online.unwrap_or(true))
        .filter_map(|peer| tailscale_peer_candidate(peer, port))
        .collect()
}

fn tailscale_peer_candidate(peer: &TailscalePeer, port: u16) -> Option<ProbeCandidate> {
    let address = peer
        .tailscale_ips
        .iter()
        .filter_map(|value| value.parse::<IpAddr>().ok())
        .min_by_key(|address| (!matches!(address, IpAddr::V4(_)), address.to_string()))?;
    let url = match address {
        IpAddr::V4(address) => format!("http://{address}:{port}"),
        IpAddr::V6(address) => format!("http://[{address}]:{port}"),
    };
    let hostname = peer
        .dns_name
        .as_deref()
        .or(peer.host_name.as_deref())
        .unwrap_or("tailscale-peer")
        .trim_end_matches('.')
        .to_string();

    Some(ProbeCandidate {
        source_id: format!("probe:tailscale:{hostname}"),
        hostname,
        url,
    })
}

#[derive(Clone, Debug)]
struct ProbeCandidate {
    source_id: String,
    hostname: String,
    url: String,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct LocalIpv4Subnet {
    addr: Ipv4Addr,
    addr_u32: u32,
    mask: u32,
}

impl LocalIpv4Subnet {
    fn host_count(self) -> u32 {
        let prefix = self.mask.leading_ones();
        if prefix >= 31 {
            0
        } else {
            (1_u32 << (32 - prefix)) - 2
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscaleStatus {
    #[serde(default)]
    peer: std::collections::HashMap<String, TailscalePeer>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TailscalePeer {
    #[serde(default)]
    host_name: Option<String>,
    #[serde(default, rename = "DNSName")]
    dns_name: Option<String>,
    #[serde(default, rename = "TailscaleIPs")]
    tailscale_ips: Vec<String>,
    #[serde(default)]
    online: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerHealth {
    ok: bool,
    app: String,
    device_id: String,
    user_id: String,
    display_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkerCapabilities {
    capabilities: Option<Vec<String>>,
    auth: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_online_tailscale_peer_candidates() {
        let status: TailscaleStatus = serde_json::from_value(serde_json::json!({
            "Peer": {
                "nodekey:one": {
                    "HostName": "mars",
                    "DNSName": "mars.tail.example.ts.net.",
                    "TailscaleIPs": ["fd7a:115c:a1e0::1", "100.84.248.34"],
                    "Online": true
                },
                "nodekey:offline": {
                    "HostName": "offline",
                    "TailscaleIPs": ["100.1.2.3"],
                    "Online": false
                }
            }
        }))
        .expect("status");

        let candidates = tailscale_peer_candidates_from_status(&status, 9109);

        assert_eq!(candidates.len(), 1);
        assert_eq!(
            candidates[0].source_id,
            "probe:tailscale:mars.tail.example.ts.net"
        );
        assert_eq!(candidates[0].hostname, "mars.tail.example.ts.net");
        assert_eq!(candidates[0].url, "http://100.84.248.34:9109");
    }

    #[test]
    fn local_subnet_probe_candidates_are_bounded_to_24() {
        let subnet = LocalIpv4Subnet {
            addr: Ipv4Addr::new(10, 10, 4, 20),
            addr_u32: u32::from(Ipv4Addr::new(10, 10, 4, 20)),
            mask: u32::from(Ipv4Addr::new(255, 255, 0, 0)),
        };

        let candidates = probe_candidates_for_ipv4_subnet(subnet, 9109);

        assert_eq!(candidates.len(), 253);
        assert!(candidates
            .iter()
            .any(|candidate| candidate.url == "http://10.10.4.1:9109"));
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.url == "http://10.10.4.20:9109"));
        assert!(candidates
            .iter()
            .any(|candidate| candidate.url == "http://10.10.4.254:9109"));
        assert!(!candidates
            .iter()
            .any(|candidate| candidate.url == "http://10.10.5.1:9109"));
    }
}
