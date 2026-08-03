/// Compile-time configuration from cfg.toml (see cfg.toml.example).
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    /// Where log lines go once wifi is up. Broadcast by default so a collector
    /// can listen from anywhere on the LAN without the bridge knowing its
    /// address — the USB console is gone once TinyUSB claims the port.
    #[default("255.255.255.255")]
    log_host: &'static str,
    #[default(49510)]
    log_port: u16,
    /// Grace period before TinyUSB claims the USB PHY. Only needed on boards
    /// whose sole flashing route is that same port; with a separate UART bridge
    /// this can be short or zero.
    #[default(500)]
    usb_start_delay_ms: u64,
    /// UDP port listening for a reboot request, which reopens the flash window
    /// without touching the BOOT button.
    #[default(49520)]
    reboot_port: u16,
    /// First DMX channel of the traffic-light fixture. Channels are 1-based, so
    /// this reads base, base+1, base+2 as red, green, yellow.
    #[default(1)]
    dmx_base_channel: u16,
    /// How many channels to forward, starting at the base. Three covers the
    /// fixture; more lets a script react to the rest of the show.
    #[default(3)]
    dmx_channel_count: u8,
    /// Where the traffic light listens. Give it a DHCP reservation.
    #[default("255.255.255.255")]
    light_host: &'static str,
    #[default(49500)]
    light_port: u16,
    /// Resend even when nothing changed, so the light can tell the bridge is
    /// still alive rather than holding the last frame forever.
    #[default(250)]
    keepalive_ms: u64,
}
