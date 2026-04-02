use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::sync::{Mutex, RwLock};

use crate::models::{ControllerInfo, StateInput, WledFullJson};

pub struct WledClient {
    http: reqwest::Client,
    locks: Arc<RwLock<HashMap<String, Arc<Mutex<()>>>>>,
}

impl WledClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to build HTTP client"),
            locks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    async fn get_lock(&self, controller_id: &str) -> Arc<Mutex<()>> {
        let locks = self.locks.read().await;
        if let Some(lock) = locks.get(controller_id) {
            return lock.clone();
        }
        drop(locks);

        let mut locks = self.locks.write().await;
        locks
            .entry(controller_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    fn base_url(controller: &ControllerInfo) -> String {
        format!("http://{}:{}", controller.ip, controller.port)
    }

    /// Fetch full JSON (state + info) without locking — used during discovery.
    pub async fn get_full_json_raw(&self, ip: &str, port: u16) -> Result<WledFullJson> {
        let url = format!("http://{ip}:{port}/json");
        let resp = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?;
        let json = resp
            .json::<WledFullJson>()
            .await
            .with_context(|| format!("parsing response from {url}"))?;
        Ok(json)
    }

    /// Fetch full JSON (state + info) for a known controller.
    pub async fn get_full_json(&self, controller: &ControllerInfo) -> Result<WledFullJson> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/json", Self::base_url(controller));
        let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let json = resp.json::<WledFullJson>().await.with_context(|| format!("parsing {url}"))?;
        Ok(json)
    }

    /// POST state to a controller. Returns the raw JSON response.
    pub async fn post_state(
        &self,
        controller: &ControllerInfo,
        state: &StateInput,
    ) -> Result<serde_json::Value> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/json/state", Self::base_url(controller));
        let resp = self
            .http
            .post(&url)
            .json(state)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let json = resp
            .json::<serde_json::Value>()
            .await
            .with_context(|| format!("parsing response from POST {url}"))?;
        Ok(json)
    }

    /// Fetch full JSON as raw serde_json::Value (for passing through to the LLM).
    pub async fn get_raw_json(&self, controller: &ControllerInfo) -> Result<serde_json::Value> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/json", Self::base_url(controller));
        let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let json = resp.json::<serde_json::Value>().await.with_context(|| format!("parsing {url}"))?;
        Ok(json)
    }

    /// Fetch device configuration (includes timers, hardware, etc).
    pub async fn get_config(&self, controller: &ControllerInfo) -> Result<serde_json::Value> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/json/cfg", Self::base_url(controller));
        let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let json = resp.json::<serde_json::Value>().await.with_context(|| format!("parsing {url}"))?;
        Ok(json)
    }

    /// POST configuration update to a controller.
    pub async fn post_config(
        &self,
        controller: &ControllerInfo,
        config: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/json/cfg", Self::base_url(controller));
        let resp = self
            .http
            .post(&url)
            .json(config)
            .send()
            .await
            .with_context(|| format!("POST {url}"))?;
        let json = resp
            .json::<serde_json::Value>()
            .await
            .with_context(|| format!("parsing response from POST {url}"))?;
        Ok(json)
    }

    /// Fetch presets.json from a controller.
    pub async fn get_presets(&self, controller: &ControllerInfo) -> Result<serde_json::Value> {
        let lock = self.get_lock(&controller.id).await;
        let _guard = lock.lock().await;

        let url = format!("{}/presets.json", Self::base_url(controller));
        let resp = self.http.get(&url).send().await.with_context(|| format!("GET {url}"))?;
        let json = resp
            .json::<serde_json::Value>()
            .await
            .with_context(|| format!("parsing {url}"))?;
        Ok(json)
    }
}
