use anyhow::{anyhow, Result};
use hyper::{Body, Client, Method, Request, StatusCode};
use hyperlocal::{UnixClientExt, Uri};
use serde::Serialize;
use serde_json::Value;

pub struct FcClient {
    socket_path: String,
    client: Client<hyperlocal::UnixConnector>,
}

impl FcClient {
    pub fn new(socket_path: &str) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            client: Client::unix(),
        }
    }

    // ── Internal ────────────────────────────────────────────────

    async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let url = Uri::new(&self.socket_path, path).into();
        let json = serde_json::to_string(body)?;

        let req = Request::builder()
            .method(Method::PUT)
            .uri(url)
            .header("Content-Type", "application/json")
            .body(Body::from(json))?;

        let resp = self.client.request(req).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let bytes = hyper::body::to_bytes(resp.into_body()).await?;
            let text = String::from_utf8_lossy(&bytes);
            return Err(anyhow!("API error {}: {}", status, text));
        }

        Ok(())
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let url = Uri::new(&self.socket_path, path).into();
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Body::empty())?;

        let resp = self.client.request(req).await?;
        let bytes = hyper::body::to_bytes(resp.into_body()).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    // ── Boot source ─────────────────────────────────────────────

    pub async fn set_boot_source(
        &self,
        kernel_path: &str,
        boot_args: &str,
    ) -> Result<()> {
        self.put("/boot-source", &serde_json::json!({
            "kernel_image_path": kernel_path,
            "boot_args": boot_args
        }))
        .await
    }

    // ── Drives ───────────────────────────────────────────────────

    pub async fn add_drive(
        &self,
        drive_id: &str,
        path: &str,
        is_root: bool,
        read_only: bool,
    ) -> Result<()> {
        self.put(&format!("/drives/{}", drive_id), &serde_json::json!({
            "drive_id":       drive_id,
            "path_on_host":   path,
            "is_root_device": is_root,
            "is_read_only":   read_only
        }))
        .await
    }

    // ── Machine config ───────────────────────────────────────────

    pub async fn set_machine_config(
        &self,
        vcpu_count: u32,
        mem_size_mib: u32,
    ) -> Result<()> {
        self.put("/machine-config", &serde_json::json!({
            "vcpu_count":    vcpu_count,
            "mem_size_mib":  mem_size_mib
        }))
        .await
    }

    // ── Network ──────────────────────────────────────────────────

    pub async fn add_network_interface(
        &self,
        iface_id: &str,
        host_dev: &str,
    ) -> Result<()> {
        self.put(&format!("/network-interfaces/{}", iface_id), &serde_json::json!({
            "iface_id":      iface_id,
            "host_dev_name": host_dev
        }))
        .await
    }

    // ── Actions ──────────────────────────────────────────────────

    pub async fn start_instance(&self) -> Result<()> {
        self.put("/actions", &serde_json::json!({
            "action_type": "InstanceStart"
        }))
        .await
    }

    // ── Info ─────────────────────────────────────────────────────

    pub async fn instance_info(&self) -> Result<Value> {
        self.get("/").await
    }

    pub async fn machine_config(&self) -> Result<Value> {
        self.get("/machine-config").await
    }
}
