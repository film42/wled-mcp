use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::models::ControllerInfo;
use crate::wled::WledClient;

const SERVICE_TYPE: &str = "_wled._tcp.local.";

pub struct ControllerRegistry {
    controllers: Arc<RwLock<HashMap<String, ControllerInfo>>>,
    #[allow(dead_code)]
    client: Arc<WledClient>,
    mdns: ServiceDaemon,
}

impl ControllerRegistry {
    pub fn new(client: Arc<WledClient>) -> Self {
        let mdns = ServiceDaemon::new().expect("failed to create mDNS daemon");
        let receiver = mdns.browse(SERVICE_TYPE).expect("failed to browse mDNS");

        let controllers: Arc<RwLock<HashMap<String, ControllerInfo>>> =
            Arc::new(RwLock::new(HashMap::new()));

        // Spawn a background task that continuously listens for mDNS events
        // and fetches controller info when new services are resolved.
        let bg_controllers = controllers.clone();
        let bg_client = client.clone();
        tokio::spawn(async move {
            loop {
                match receiver.recv_async().await {
                    Ok(ServiceEvent::ServiceResolved(service_info)) => {
                        let port = service_info.get_port();
                        for addr in service_info.get_addresses_v4() {
                            let ip = IpAddr::V4(addr);
                            info!("mDNS resolved WLED at {ip}:{port}");
                            match bg_client.get_full_json_raw(&ip.to_string(), port).await {
                                Ok(full_json) => {
                                    let wled_info = &full_json.info;
                                    let controller = ControllerInfo {
                                        id: wled_info.mac.clone(),
                                        name: wled_info.name.clone(),
                                        ip,
                                        port,
                                        led_count: wled_info.leds.count,
                                        rgbw: wled_info.leds.rgbw,
                                        firmware: wled_info.ver.clone(),
                                        max_segments: wled_info.leds.maxseg,
                                    };
                                    info!(
                                        "registered controller '{}' ({}), {} LEDs",
                                        controller.name, controller.id, controller.led_count
                                    );
                                    bg_controllers
                                        .write()
                                        .await
                                        .insert(controller.id.clone(), controller);
                                }
                                Err(e) => {
                                    warn!("failed to query WLED at {ip}:{port}: {e}");
                                }
                            }
                        }
                    }
                    Ok(ServiceEvent::ServiceRemoved(_, fullname)) => {
                        info!("mDNS service removed: {fullname}");
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("mDNS receiver error: {e}");
                        break;
                    }
                }
            }
        });

        Self {
            controllers,
            client,
            mdns,
        }
    }

    /// Look up a controller by MAC-based ID.
    pub async fn get(&self, id: &str) -> Option<ControllerInfo> {
        self.controllers.read().await.get(id).cloned()
    }

    /// Return all cached controllers.
    #[allow(dead_code)]
    pub async fn all(&self) -> Vec<ControllerInfo> {
        self.controllers.read().await.values().cloned().collect()
    }

    /// Return discovered controllers. If the cache is empty, wait briefly for
    /// mDNS responses to arrive. Otherwise returns immediately from cache.
    pub async fn discover(&self) -> Result<Vec<ControllerInfo>> {
        // If cache already has entries, return them right away
        {
            let cache = self.controllers.read().await;
            if !cache.is_empty() {
                return Ok(cache.values().cloned().collect());
            }
        }

        // Cache is empty — wait for the background listener to populate it.
        // Poll every 250ms for up to 5 seconds.
        let timeout_secs: u64 = std::env::var("WLED_DISCOVERY_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);

        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);

        loop {
            tokio::time::sleep(Duration::from_millis(250)).await;

            let cache = self.controllers.read().await;
            if !cache.is_empty() {
                return Ok(cache.values().cloned().collect());
            }

            if tokio::time::Instant::now() >= deadline {
                break;
            }
        }

        // Last check
        let cache = self.controllers.read().await;
        Ok(cache.values().cloned().collect())
    }
}

impl Drop for ControllerRegistry {
    fn drop(&mut self) {
        let _ = self.mdns.shutdown();
    }
}
