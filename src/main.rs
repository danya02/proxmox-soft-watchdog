mod api;
mod config;

#[tokio::main]
async fn main() {
    let file_name = std::env::args()
        .nth(1)
        .expect("first argument needs to be a file path to config.json");

    let config_text = std::fs::read_to_string(file_name).expect("cannot read config file");
    let config: config::Config = serde_json::from_str(&config_text).expect("cannot parse config");

    println!("{:#?}", config);
}
