use anyhow::{anyhow, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request, StatusCode};
use hyperlocal::{UnixClientExt, Uri};
use hyper_util::client::legacy::Client;
use serde::Serialize;
use serde_json::Value;

pub struct FcClient {
    socket_path: String,
    client: Client<hyperlocal::UnixConnector, Full<Bytes>>,
    dry_run: bool,
}

impl FcClient {
    pub fn new(socket_path: &str) -> Self {
        Self::new_with_mode(socket_path, false)
    }

    pub fn new_dry_run(socket_path: &str) -> Self {
        Self::new_with_mode(socket_path, true)
    }
    
    // We can replace the new methode above with this one to set up the dry_run option.
    fn new_with_mode(socket_path: &str, dry_run: bool) -> Self {
        Self {
            socket_path: socket_path.to_string(),
            client: Client::unix(),
            dry_run,
        }
    }

    fn print_request(&self, method: &str, path: &str, body: Option<&str>) {
        let url = format!("unix://{}{}", self.socket_path, path);
        match body {
            Some(body) => println!("[DRY RUN] {} {}\n{}", method, url, body),
            None => println!("[DRY RUN] {} {}", method, url),
        }
    }

    // ── Internal ────────────────────────────────────────────────

    async fn put<T: Serialize>(&self, path: &str, body: &T) -> Result<()> {
        let json = serde_json::to_string(body)?;
        if self.dry_run {
            self.print_request("PUT", path, Some(&json));
            return Ok(());
        }
        let url:Uri = Uri::new(&self.socket_path, path).into();

        let req = Request::builder()
            .method(Method::PUT)
            .uri(url)
            .header("Content-Type", "application/json")
            .body(Full::new(Bytes::from(json)))?;

        let resp = self.client.request(req).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let bytes = resp.into_body().collect().await?.to_bytes();
            // let bytes = hyper::body::to_bytes(resp.into_body()).await?;
            let text = String::from_utf8_lossy(&bytes);
            return Err(anyhow!("API error {}: {}", status, text));
        }

        Ok(())
    }

    async fn get(&self, path: &str) -> Result<Value> {
        if self.dry_run {
            self.print_request("GET", path, None);
            return Ok(serde_json::json!({ "dry_run": true, "path": path }));
        }
        let url:Uri = Uri::new(&self.socket_path, path).into();
        let req = Request::builder()
            .method(Method::GET)
            .uri(url)
            .body(Full::new(Bytes::new()))?;

        let resp = self.client.request(req).await?;
        let bytes = resp.into_body().collect().await?.to_bytes();
        // let bytes = hyper::body::to_bytes(resp.into_body()).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn prints_api_requests_in_dry_run() -> Result<()> {
        let client = FcClient::new_dry_run("/tmp/fc-bench.socket");

        client
            .set_boot_source(
                "/path/to/vmlinux",
                "console=ttyS0 reboot=k panic=1 pci=off benchmark=rand_read",
            )
            .await?;
        client
            .add_drive("rootfs", "/path/to/rootfs.ext4", true, false)
            .await?;
        client
            .add_drive("benchdisk", "/path/to/bench.raw", false, false)
            .await?;
        client.set_machine_config(2, 512).await?;
        client.add_network_interface("eth0", "tap0").await?;
        client.start_instance().await?;

        let _ = client.instance_info().await?;
        let _ = client.machine_config().await?;

        Ok(())
    }
}
