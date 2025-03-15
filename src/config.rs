use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub proxmox_auth: ProxmoxAuth,
    pub vm_configs: Vec<VmConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxmoxAuth {
    pub url: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmConfig {
    pub host_name: String,
    pub vmid: String,
    pub friendly_name: String,

    /// The maximum time from now that the VM can request,
    /// before we send a warning.
    /// In seconds.
    /// Something like 30 minutes is reasonable.
    pub max_no_warning_interval: u64,

    /// When the requested time has passed,
    /// we start a countdown whose duration is this.
    /// If the VM does not respond in time,
    /// we reset it.
    pub grace_period: u64,

    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,

    /// If this is true, then enforcing will not happen.
    /// Instead, we'll send a message if we would reset the VM.
    pub dry_run: bool,
}
