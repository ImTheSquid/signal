# dmx-bridge

An ESP32-S3 that pretends to be an Enttec DMX interface so **rekordbox lighting** can drive
the traffic light. It presents FTDI USB descriptors over the native USB port, reassembles the
DMX512 stream rekordbox writes to it, and forwards the channel values to the light over LAN
UDP.

```
rekordbox (Mac) --USB--> ESP32-S3                --wifi/UDP--> traffic light
                         TinyUSB, FTDI descriptors                 relays
                         DMX frame reassembly
```

## Why this shape

rekordbox has no network lighting output — no Art-Net, no sACN. Since 7.0.7 it accepts
third-party Enttec interfaces (Open DMX, DMX USB Pro, Pro Mk2), all of which are FTDI-based
and use FTDI's stock `0403:6001`. Preferences offers a single "ENTTEC DMX-IF" toggle rather
than per-model entries, so it opens any FTDI device and probes. Answering that probe is all
it takes.

Three other routes were tried and rejected:

- **Ableton Link** works, but enabling it in rekordbox hands master tempo to the Link session
  and disables the deck tempo faders.
- **Pro DJ Link** never transmits from a DDJ-only rig. Three tshark captures, control-verified
  against mDNS/ARP broadcast, with a peer playing on the same subnet and PRO DJ LINK Lighting
  enabled: zero packets on 50000-50002. rekordbox binds all three ports but only listens, and
  is documented as master clock only in **export mode**, which you can't be in while performing.
- **MIDI clock** does not exist in rekordbox.

## What rekordbox actually sends

Measured, not assumed. It drives this as an **Enttec Open DMX USB** — raw DMX512 on the
serial link, not the Pro widget protocol. There is no `0x7E` framing and no "Get Widget
Parameters" handshake.

Exactly two control requests appear, alternating twice per frame:

| `SET_DATA` wValue | meaning |
|---|---|
| `0x1008` | 8 data bits, no parity, **2 stop bits**, BREAK off |
| `0x5008` | same framing, BREAK **on** (bit 14) |

Then the bulk endpoint carries one 1-byte start code followed by 512 channel bytes — a fixed
**513 bytes per frame**, at roughly 41 Hz.

On open it also issues `SIO_RESET` (0, then 1/1 to purge RX/TX, then 2), `GET_MODEM_STATUS`
and `GET_LATENCY_TIMER`. Plausible canned replies are enough.

### Frame delimiting: use the byte count, not the BREAK

Tempting and wrong: delimit frames on the BREAK edge. The edge is recorded in the USB task
while bytes are drained on the main loop, with no ordering between them, so an edge observed
mid-drain truncates a healthy frame. Measured at **38% of frames corrupted**, with lengths
scattered from 64 (one USB packet) up to 513.

Frames are a fixed 513 bytes, so the byte count is the delimiter. The BREAK edge is used
exactly twice: to align the first frame, and to detect drift afterwards (one edge per frame;
sustained divergence forces a re-align). That gives `resyncs=0` and `len=513..513`.

## Wire format to the light

Raw channel values, not a lamp decision — thresholding and the channel-to-lamp mapping belong
to the Rhai script on the light, so both can change without reflashing either device.

```
off size field
0   2    magic "TL"
2   1    version (3)
3   1    header_len (10)
4   4    seq, u32 BE
8   2    base channel, u16 BE
10  ..   channel values, one byte each, to the end of the datagram
```

`header_len` is what makes this extensible: the light skips header fields it does not
recognise, so a newer bridge can append without a light reflash — which matters because the
light needs physical access to update. The channel count is implied by datagram length, so it
can never disagree with the payload.

Unauthenticated by design, like Ableton Link on the same LAN. It is narrower than it looks:
the light only opens its socket while a script is calling `dmx_recv()`, and that script only
runs while its submitter holds the lock.

## Board profiles

The S3's USB-OTG and USB-Serial-JTAG **share one PHY**, so TinyUSB taking the native port
removes the console and espflash's route in.

**Two USB ports (recommended).** Flash and monitor over the UART bridge while the native port
does FTDI. Requires `CONFIG_ESP_CONSOLE_SECONDARY_NONE=y`, already in `sdkconfig.defaults` —
without it the secondary console sits on the PHY TinyUSB claims, nothing drains its FIFO, and
the first `println!` after USB init **blocks forever**, which looks like a total silent hang.

**Single USB port.** The console goes to UART0 pins (harmless if nothing is attached; UDP
logging covers it). To flash, either raise `usb_start_delay_ms` above the ~12 s a flash takes,
or send the reboot command:

```sh
printf 'dmx-bridge-reboot' | nc -u -w1 <bridge-ip> 49520
```

Unicast, not broadcast — macOS `nc` leaves `SO_BROADCAST` off. The handler calls
`tinyusb_driver_uninstall()` before restarting: `esp_restart()` alone does **not** reset the
USB PHY, so the ROM cannot bring USB-Serial-JTAG back and the board vanishes from the bus
until physically power-cycled.

## Logging

Console plus UDP broadcast on `log_port` (49510), because on a single-PHY board the console is
gone once USB is claimed. Lines emitted before wifi is up are buffered and replayed when the
sink attaches, so a failure during wifi bring-up is still visible.

```sh
node ../scripts/logcat.mjs 49510   # or any UDP listener
```

## Setup

```sh
cp cfg.toml.example cfg.toml    # wifi, light address, channel window
cargo build --release
espflash flash --port /dev/cu.usbmodemXXXX target/xtensa-esp32s3-espidf/release/dmx-bridge
```

`.embuild` is symlinked to `../firmware/.embuild` so both projects share one ESP-IDF checkout
and toolchain (~4 GB). Don't build both at once — they share that tree. Delete the symlink for
a private copy.

In rekordbox: enable **ENTTEC DMX-IF** under Preferences → Extensions, patch a 3-channel RGB
fixture at `dmx_base_channel`, and make sure *"Setting of Venue to play Macro"* names the venue
the fixture is actually in. Use **Delay Compensation for Lighting** (±500 ms) to absorb USB,
wifi, relay actuation and lamp rise in one place.

## Gotchas

- Reflashing re-enumerates the USB device and **rekordbox silently drops it** — re-toggle
  ENTTEC DMX-IF afterwards. Repeated hot-unplugs also appear to crash rekordbox, so prefer the
  reboot command (clean teardown) over yanking it.
- All channels zero with frames arriving at ~40 fps means the engine is running but outputting
  blackout: nothing playing (and Auto Start off), or a venue mismatch.
- `esp_tinyusb` is pinned to `^1.7`. esp-idf-sys 0.37's `bindings.h` includes
  `tinyusb_types.h`, which 2.x dropped.
- `components/tinyusb_bindgen_shim` exists only so bindgen can resolve TinyUSB's FreeRTOS
  includes: `osal_freertos.h` reaches them through `TU_INCLUDE_PATH(CFG_TUSB_OS_INC_PATH, ...)`,
  a define private to the tinyusb component, so bindgen expands it to a bare `FreeRTOS.h`.
