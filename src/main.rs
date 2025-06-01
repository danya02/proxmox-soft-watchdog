use tracing::Instrument;

mod api;
mod config;
pub mod monitoring;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let file_name = std::env::args()
        .nth(1)
        .expect("first argument needs to be a file path to config.json");

    let config_text = std::fs::read_to_string(file_name).expect("cannot read config file");
    let config: config::Config = serde_json::from_str(&config_text).expect("cannot parse config");

    println!("{:#?}", config);

    let api = api::Api::from_config(&config.proxmox_auth);

    api.get_ticket().await;

    for vm_config in config.vm_configs {
        tokio::spawn(test_single_vm(api.clone(), vm_config));
    }

    tokio::signal::ctrl_c().await.unwrap();
}

async fn test_single_vm(api: api::Api, vm_config: config::VmConfig) {
    let mut monitor = monitoring::SingleMachineMonitoring::new(api.clone(), vm_config.clone());
    monitor.say("Monitoring loop started!").await;
    loop {
        let vmid = &vm_config.vmid;
        monitor
            .tick()
            .instrument(tracing::info_span!("monitoring tick", vmid = vmid))
            .await;
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}
