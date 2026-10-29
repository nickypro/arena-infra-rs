//! The image commands' Hetzner API seam: [`ImageCloud`], implemented by the fleet's own
//! [`crate::provider::hetzner::HetznerProvider`] (its token, its location walk for the bake VM's
//! create, its graceful stop), and (`cfg(test)` / feature `test-util`) a stateful
//! [`FakeImageCloud`] that records every call. Listings fail closed, as the provider's do: a page
//! without its array is an error, never "no images" — prune and teardown read it.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Map, Value};

use super::{ImagePrice, Snapshot};
use crate::error::Result;
use crate::pod::Pod;
use crate::provider::hetzner::ServerType;

#[async_trait]
pub trait ImageCloud: Send + Sync {
    /// The project's snapshots (`GET /images?type=snapshot`), only those matching the label
    /// `selector` when given.
    async fn snapshots(&self, selector: Option<&str>) -> Result<Vec<Snapshot>>;
    /// The server types on sale ([`crate::provider::hetzner::HetznerProvider::catalog`]).
    async fn server_types(&self) -> Result<Vec<ServerType>>;
    /// What a GB of snapshot costs a month (`GET /pricing`).
    async fn image_price(&self) -> Result<ImagePrice>;
    /// Create the bake VM from Ubuntu with `labels` — the fleet's create (location walk, the
    /// cohort's SSH key).
    async fn create_server(&self, name: &str, server_type: &str, labels: Map<String, Value>) -> Result<Pod>;
    /// One server, `None` once it's gone (404).
    async fn server(&self, id: &str) -> Result<Option<Pod>>;
    /// The servers matching a label `selector`.
    async fn servers(&self, selector: &str) -> Result<Vec<Pod>>;
    /// Shut the server down (ACPI, then a power-off if it doesn't), and wait until it's off.
    async fn stop(&self, id: &str) -> Result<()>;
    /// `POST /servers/{id}/actions/create_image` (type snapshot): the image id and its action.
    async fn create_image(&self, server: &str, description: &str, labels: &Map<String, Value>) -> Result<(u64, Option<u64>)>;
    /// Wait for an action to succeed (an error if it fails or `limit` passes).
    async fn wait_action(&self, action: u64, limit: Duration) -> Result<()>;
    /// One image, `None` once it's gone.
    async fn image(&self, id: u64) -> Result<Option<Snapshot>>;
    async fn delete_server(&self, id: &str) -> Result<()>;
    async fn delete_image(&self, id: u64) -> Result<()>;
}

#[cfg(any(test, feature = "test-util"))]
pub use fake::{FakeImageCloud, ImageCall};

#[cfg(any(test, feature = "test-util"))]
mod fake {
    use std::sync::Mutex;

    use super::*;
    use crate::error::Error;

    /// One recorded call.
    #[derive(Debug, Clone, PartialEq)]
    pub enum ImageCall {
        Snapshots(Option<String>),
        ServerTypes,
        Price,
        CreateServer { name: String, server_type: String, labels: Map<String, Value> },
        Server(String),
        Servers(String),
        Stop(String),
        CreateImage { server: String, description: String, labels: Map<String, Value> },
        WaitAction(u64),
        Image(u64),
        DeleteServer(String),
        DeleteImage(u64),
    }

    /// A Hetzner project in memory: snapshots, servers and types; failures to order.
    #[derive(Debug, Default)]
    pub struct FakeImageCloud {
        pub snaps: Mutex<Vec<Snapshot>>,
        pub servers: Mutex<Vec<Pod>>,
        pub types: Mutex<Vec<ServerType>>,
        pub calls: Mutex<Vec<ImageCall>>,
        /// The next N deletes of a server fail.
        pub fail_server_deletes: Mutex<usize>,
        /// `create_image`'s action fails.
        pub fail_snapshot: Mutex<bool>,
        /// `create_server` fails with this.
        pub fail_create: Mutex<Option<String>>,
        /// `create_server` takes this long (tokio time).
        pub create_delay: Mutex<Duration>,
        next_id: Mutex<u64>,
    }

    impl FakeImageCloud {
        pub fn new(types: Vec<ServerType>) -> Self {
            let f = Self::default();
            *f.types.lock().unwrap() = types;
            *f.next_id.lock().unwrap() = 1000;
            f
        }

        pub fn calls(&self) -> Vec<ImageCall> {
            self.calls.lock().unwrap().clone()
        }

        fn record(&self, c: ImageCall) {
            self.calls.lock().unwrap().push(c);
        }

        fn id(&self) -> u64 {
            let mut n = self.next_id.lock().unwrap();
            *n += 1;
            *n
        }
    }

    #[async_trait]
    impl ImageCloud for FakeImageCloud {
        async fn snapshots(&self, selector: Option<&str>) -> Result<Vec<Snapshot>> {
            self.record(ImageCall::Snapshots(selector.map(String::from)));
            Ok(self.snaps.lock().unwrap().clone())
        }
        async fn server_types(&self) -> Result<Vec<ServerType>> {
            self.record(ImageCall::ServerTypes);
            Ok(self.types.lock().unwrap().clone())
        }
        async fn image_price(&self) -> Result<ImagePrice> {
            self.record(ImageCall::Price);
            Ok(ImagePrice { per_gb_month: 0.0119, currency: "EUR".into() })
        }
        async fn create_server(&self, name: &str, server_type: &str, labels: Map<String, Value>) -> Result<Pod> {
            self.record(ImageCall::CreateServer { name: name.into(), server_type: server_type.into(), labels });
            let delay = *self.create_delay.lock().unwrap();
            tokio::time::sleep(delay).await;
            if let Some(why) = self.fail_create.lock().unwrap().clone() {
                return Err(Error::provider(why));
            }
            let id = self.id();
            let pod = Pod {
                id: id.to_string(),
                name: name.into(),
                provider: "hetzner".into(),
                status: "RUNNING".into(),
                ssh_ip: Some(format!("10.0.0.{}", id % 250)),
                ssh_port: Some(22),
                gpu_type: Some(server_type.into()),
                ..Pod::default()
            };
            self.servers.lock().unwrap().push(pod.clone());
            Ok(pod)
        }
        async fn server(&self, id: &str) -> Result<Option<Pod>> {
            self.record(ImageCall::Server(id.into()));
            Ok(self.servers.lock().unwrap().iter().find(|p| p.id == id).cloned())
        }
        async fn servers(&self, selector: &str) -> Result<Vec<Pod>> {
            self.record(ImageCall::Servers(selector.into()));
            Ok(self.servers.lock().unwrap().clone())
        }
        async fn stop(&self, id: &str) -> Result<()> {
            self.record(ImageCall::Stop(id.into()));
            for p in self.servers.lock().unwrap().iter_mut().filter(|p| p.id == id) {
                p.status = "OFF".into();
            }
            Ok(())
        }
        async fn create_image(&self, server: &str, description: &str, labels: &Map<String, Value>) -> Result<(u64, Option<u64>)> {
            self.record(ImageCall::CreateImage { server: server.into(), description: description.into(), labels: labels.clone() });
            let id = self.id();
            let snap = super::super::parse_snapshot(&serde_json::json!({
                "id": id, "description": description, "status": "available", "image_size": 5.8, "disk_size": 40,
                "architecture": "x86", "labels": labels, "created": "2026-10-29T12:00:00Z"
            }))
            .expect("a snapshot");
            self.snaps.lock().unwrap().push(snap);
            Ok((id, Some(id + 500_000)))
        }
        async fn wait_action(&self, action: u64, _limit: Duration) -> Result<()> {
            self.record(ImageCall::WaitAction(action));
            if action > 500_000 && *self.fail_snapshot.lock().unwrap() {
                return Err(Error::provider(format!("hetzner create_image (action {action}) failed: disk error")));
            }
            Ok(())
        }
        async fn image(&self, id: u64) -> Result<Option<Snapshot>> {
            self.record(ImageCall::Image(id));
            Ok(self.snaps.lock().unwrap().iter().find(|s| s.id == id).cloned())
        }
        async fn delete_server(&self, id: &str) -> Result<()> {
            self.record(ImageCall::DeleteServer(id.into()));
            let mut fail = self.fail_server_deletes.lock().unwrap();
            if *fail > 0 {
                *fail -= 1;
                return Err(Error::provider("hetzner delete server: 503 unavailable"));
            }
            self.servers.lock().unwrap().retain(|p| p.id != id);
            Ok(())
        }
        async fn delete_image(&self, id: u64) -> Result<()> {
            self.record(ImageCall::DeleteImage(id));
            self.snaps.lock().unwrap().retain(|s| s.id != id);
            Ok(())
        }
    }
}
