//! Vast.ai backend — stubbed against the same [`Provider`] trait so the rest of
//! the system already treats it as a first-class provider. Implementing it is the
//! next vertical: Vast exposes a REST API at https://console.vast.ai/api/v0 with a
//! search/offers model (you bid on instances) rather than RunPod's named-pod model,
//! so `create_pod` will map a `PodSpec` onto an offer search + `PUT /asks/{id}`.

use async_trait::async_trait;

use super::Provider;
use crate::error::{Error, Result};
use crate::pod::{Pod, PodSpec};

pub struct VastProvider {
    #[allow(dead_code)]
    api_key: String,
}

impl VastProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
        }
    }
}

#[async_trait]
impl Provider for VastProvider {
    fn name(&self) -> &'static str {
        "vast"
    }

    async fn list_pods(&self) -> Result<Vec<Pod>> {
        Err(Error::NotImplemented("vast: list_pods".into()))
    }

    async fn create_pod(&self, _spec: &PodSpec) -> Result<Pod> {
        Err(Error::NotImplemented("vast: create_pod".into()))
    }

    async fn stop_pod(&self, _id: &str) -> Result<()> {
        Err(Error::NotImplemented("vast: stop_pod".into()))
    }

    async fn terminate_pod(&self, _id: &str) -> Result<()> {
        Err(Error::NotImplemented("vast: terminate_pod".into()))
    }
}
