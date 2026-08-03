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
    /// Minimum time a lamp holds a state before the relay may move again.
    /// Bounds contact wear and keeps scripts inside what the relay can
    /// mechanically follow (operate + release is roughly 15ms).
    #[default(100)]
    min_lamp_dwell_ms: u64,
    /// UDP port `dmx_recv()` listens on. Deliberately not 6454 (Art-Net) or
    /// 5568 (sACN) so a DMX-adjacent LAN can't reach it by accident.
    #[default(49500)]
    dmx_port: u16,
}
