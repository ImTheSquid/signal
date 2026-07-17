/// Compile-time configuration from cfg.toml (see cfg.toml.example).
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    #[default("ws://localhost:3001")]
    ws_url: &'static str,
    #[default("")]
    device_token: &'static str,
    #[default(false)]
    active_low: bool,
}
