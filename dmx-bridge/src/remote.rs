//! Remote reboot, so reflashing doesn't need a physical BOOT+RESET.
//!
//! Once TinyUSB owns the PHY there is no serial port for espflash to open, and
//! the only way back is the button. This listens on UDP and restarts the chip,
//! which brings USB-Serial-JTAG back for `usb_start_delay_ms` — long enough to
//! flash. Development affordance: set the delay to 0 for production and this
//! becomes the only way in.

use std::net::UdpSocket;

/// Distinctive enough that no stray broadcast will ever reboot the bridge by
/// accident. Not a secret — the LAN is trusted, same as the DMX path.
const REBOOT_MAGIC: &[u8] = b"dmx-bridge-reboot";

/// Spawn the listener. Never returns an error to the caller: a bridge that can't
/// listen for reboots should still bridge DMX.
pub fn spawn(port: u16) {
    std::thread::Builder::new()
        .name("remote".into())
        .stack_size(3072)
        .spawn(move || match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(socket) => {
                log::info!("reboot listener on udp/{port} (send \"dmx-bridge-reboot\")");
                let mut buf = [0u8; 64];
                loop {
                    match socket.recv_from(&mut buf) {
                        Ok((n, from)) if &buf[..n] == REBOOT_MAGIC => {
                            log::warn!("reboot requested by {from}");
                            // Give the log datagram a chance to leave first.
                            std::thread::sleep(std::time::Duration::from_millis(150));
                            // esp_restart() alone does NOT reset the USB PHY, so
                            // the ROM cannot bring USB-Serial-JTAG back after
                            // OTG has claimed it — the board vanishes from the
                            // bus until it is physically power-cycled. Hand the
                            // PHY back before restarting.
                            unsafe {
                                esp_idf_svc::sys::tinyusb_driver_uninstall();
                                std::thread::sleep(std::time::Duration::from_millis(100));
                                esp_idf_svc::sys::esp_restart()
                            };
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::warn!("reboot listener: {e}");
                            std::thread::sleep(std::time::Duration::from_millis(500));
                        }
                    }
                }
            }
            Err(e) => log::error!("reboot listener bind failed: {e}"),
        })
        .map(|_| ())
        .unwrap_or_else(|e| log::error!("reboot listener spawn failed: {e}"));
}
