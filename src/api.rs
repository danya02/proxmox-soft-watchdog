use std::sync::{Arc, Mutex};

use base64::Engine;
use reqwest_retry::{RetryTransientMiddleware, RetryableStrategy, policies::ExponentialBackoff};

use crate::config;

type ReqError = reqwest_middleware::Error;

#[derive(Clone)]
pub struct Api {
    inner: Arc<Inner>,
    client: reqwest_middleware::ClientWithMiddleware,
}

struct Inner {
    base_url: String,
    username: String,
    password: String,
    ticket: Mutex<Option<String>>,
    csrf: Mutex<Option<String>>,
    ticket_expiry: Mutex<std::time::Instant>,
}

struct MyRetryableStrategy;

impl RetryableStrategy for MyRetryableStrategy {
    fn handle(
        &self,
        res: &Result<reqwest::Response, reqwest_middleware::Error>,
    ) -> Option<reqwest_retry::Retryable> {
        match res {
            // retry all errors in sending
            Err(_) => Some(reqwest_retry::Retryable::Transient),

            Ok(_) => {
                // Any response is considered OK
                None
            }
        }
    }
}

impl Api {
    pub fn from_config(conf: &config::ProxmoxAuth) -> Self {
        let retry_policy = ExponentialBackoff::builder()
            .retry_bounds(
                std::time::Duration::from_millis(100),
                std::time::Duration::from_secs(3),
            )
            .build_with_max_retries(3);

        Self {
            inner: Arc::new(Inner {
                base_url: conf.url.clone(),
                username: conf.user.clone(),
                password: conf.password.clone(),
                ticket: Mutex::new(None),
                csrf: Mutex::new(None),
                ticket_expiry: Mutex::new(std::time::Instant::now()),
            }),
            client: reqwest_middleware::ClientBuilder::new(
                reqwest::Client::builder()
                    .danger_accept_invalid_certs(conf.allow_invalid_cert)
                    .build()
                    .expect("failed to build reqwest client"),
            )
            .with(RetryTransientMiddleware::new_with_policy_and_strategy(
                retry_policy,
                MyRetryableStrategy,
            ))
            .build(),
        }
    }

    #[tracing::instrument(skip(self), level = "debug")]
    pub async fn get_ticket(&self) -> (String, String) {
        // If there is a cached ticket and it hasn't yet expired,
        // return it.
        let ticket_expiry = *self.inner.ticket_expiry.lock().unwrap();
        if ticket_expiry > std::time::Instant::now() {
            let ticket = self.inner.ticket.lock().unwrap().clone().unwrap();
            let csrf = self.inner.csrf.lock().unwrap().clone().unwrap();
            tracing::debug!("Reusing cached ticket");
            return (ticket, csrf);
        }

        // Copy the inner ticket,
        // and check that it works.
        let ticket = self.inner.ticket.lock().unwrap().clone();
        if let Some(ticket) = ticket {
            tracing::debug!("Testing cached ticket");
            if let Ok(res) = self
                .client
                .get(format!("{}/api2/json/access/ticket", self.inner.base_url))
                .bearer_auth(&ticket)
                .send()
                .await
            {
                if res.status().is_success() {
                    tracing::debug!("Cached ticket is still valid");
                    let csrf = self.inner.csrf.lock().unwrap().clone().unwrap();
                    *self.inner.ticket_expiry.lock().unwrap() =
                        std::time::Instant::now() + std::time::Duration::from_secs(60);
                    return (ticket, csrf);
                }
            }
        }

        // If there is no cached ticket,
        // make a new one.
        tracing::info!("Getting new ticket");
        let res = self
            .client
            .post(format!("{}/api2/json/access/ticket", self.inner.base_url))
            .json(&serde_json::json!({
                "username": self.inner.username,
                "password": self.inner.password,
            }))
            .send()
            .await
            .unwrap();

        if res.status().is_success() {
            let json: serde_json::Value = res.json().await.unwrap();
            let ticket = json["data"]["ticket"].as_str().unwrap().to_string();
            let csrf = json["data"]["CSRFPreventionToken"]
                .as_str()
                .unwrap()
                .to_string();
            self.inner.ticket.lock().unwrap().replace(ticket.clone());
            self.inner.csrf.lock().unwrap().replace(csrf.clone());
            *self.inner.ticket_expiry.lock().unwrap() =
                std::time::Instant::now() + std::time::Duration::from_secs(10 * 60);
            return (ticket, csrf);
        } else {
            panic!("failed to get ticket: {}", res.status());
        }
    }

    #[tracing::instrument(name = "ticketed_request", skip(self), level = "debug")]
    async fn ticketed_request(
        &self,
        method: reqwest::Method,
        path: &str,
    ) -> reqwest_middleware::RequestBuilder {
        let url = format!("{}/api2/json{}", self.inner.base_url, path);
        let (ticket, csrf) = self.get_ticket().await;
        self.client
            .request(method, url)
            .bearer_auth(format!("PVEAuthCookie={ticket}"))
            .header("CSRFPreventionToken", csrf)
    }

    #[tracing::instrument(skip(self, config))]
    pub async fn ping_guest_agent(&self, config: &config::VmConfig) -> Result<(), ReqError> {
        tracing::debug!("Pinging guest agent");
        let res = self
            .ticketed_request(
                reqwest::Method::POST,
                &format!("/nodes/{}/qemu/{}/agent/ping", config.node, config.vmid),
            )
            .await
            .send()
            .await?;

        // println!("VMID {} ping: {}", config.vmid, res.text().await?);
        res.error_for_status()?;
        Ok(())
    }

    #[tracing::instrument(skip(self, config, path, content))]
    pub async fn guest_agent_write_file(
        &self,
        config: &config::VmConfig,
        path: &str,
        content: &[u8],
    ) -> Result<(), ReqError> {
        tracing::debug!("Writing guest agent file {}", path);
        let content = base64::engine::general_purpose::STANDARD.encode(content);
        let res = self
            .ticketed_request(
                reqwest::Method::POST,
                &format!(
                    "/nodes/{}/qemu/{}/agent/file-write",
                    config.node, config.vmid
                ),
            )
            .await
            .json(&serde_json::json!({
                "file": path,
                "content": content,
                "encode": false
            }))
            .send()
            .await?;

        res.error_for_status()?.text().await?;

        Ok(())
    }

    #[tracing::instrument(skip(self, config, path))]
    pub async fn guest_agent_read_file(
        &self,
        config: &config::VmConfig,
        path: &str,
    ) -> Result<String, ReqError> {
        tracing::debug!("Reading guest agent file {}", path);
        let res = self
            .ticketed_request(
                reqwest::Method::GET,
                &format!(
                    "/nodes/{}/qemu/{}/agent/file-read",
                    config.node, config.vmid
                ),
            )
            .await
            .query(&[("file", path)])
            .send()
            .await?;

        let res = res.error_for_status()?;
        let json: serde_json::Value = res.json().await.unwrap();
        let content = json["data"]["content"].as_str().unwrap();

        Ok(content.to_string())
    }

    #[tracing::instrument(skip(self, config))]
    pub async fn get_is_machine_running(
        &self,
        config: &config::VmConfig,
    ) -> Result<bool, ReqError> {
        tracing::debug!("Getting VM status from hypervisor");
        let res = self
            .ticketed_request(
                reqwest::Method::GET,
                &format!("/nodes/{}/qemu/{}/status/current", config.node, config.vmid),
            )
            .await
            .send()
            .await?
            .error_for_status()?;

        let json: serde_json::Value = res.json().await.expect("failed to parse response as JSON");
        let status = json["data"]["status"].as_str().unwrap();
        Ok(status == "running")
    }

    #[tracing::instrument(skip(self, config))]
    pub async fn reset_vm(&self, config: &config::VmConfig) -> Result<(), ReqError> {
        tracing::info!("Resetting VM in hypervisor");
        let res = self
            .ticketed_request(
                reqwest::Method::POST,
                &format!("/nodes/{}/qemu/{}/status/reset", config.node, config.vmid),
            )
            .await
            .send()
            .await?;

        res.error_for_status()?;

        Ok(())
    }
}
