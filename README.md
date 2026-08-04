# traffic-light

A real three-lamp traffic signal on the internet. Friends, apps, and trinkets get API keys; a key can hold a time-limited **lock** on the light and submit a [Rhai](https://rhai.rs) script that the ESP32 executes. A public dashboard shows who holds the lock and what the lamps are doing.

## Architecture

```
friend ──HTTP──▶ SvelteKit /v1/* ──────────▶ Redis ◀────── pub/sub ────── api/device.ts
                    │                          ▲                             ▲ websocket
                    └─ validates + minifies    │ pub/sub                     └── ESP32 (runs Rhai on-device)
                       via Rhai→WASM       api/live.ts
                                               ▲ websocket
                                            dashboard (live updates)
```

Everything runs on one box behind nginx — see [deploy/home-app/](deploy/home-app/).

- **apps/web** — SvelteKit app: public dashboard (live over websocket, 10s polling as fallback), admin panel (keys, idle script editor, lamp test, history controls, kill switch), the `/v1` JSON API, and two standalone websocket servers: `api/device.ts` (the ESP32) and `api/live.ts` (browsers). SvelteKit routes can't upgrade websockets, so those run as their own processes — which is also why the JSON API lives at `/v1`, not `/api`.
- **crates/script-env** — the single source of truth for the Rhai language surface and sandbox limits, used by both the validator and the firmware.
- **crates/validator** → **packages/validator-wasm** — `Engine::compile` plus [rhaiper](https://crates.io/crates/rhaiper), compiled to WASM. `POST /v1/script` rejects scripts that won't parse, with line/col errors, and minifies the rest before storing them.
- **packages/protocol** — zod schemas + constants for the wire protocol and Redis keys.
- **firmware** — Rust (esp-idf) for an ESP32-WROOM-32E: wifi → SNTP → websocket, Rhai engine in a dedicated thread, three relay GPIOs (32/33/25 = R/Y/G).
- **dmx-bridge** — Rust (esp-idf) for an ESP32-S3 that presents FTDI descriptors over USB so **rekordbox lighting** drives the signal. It reassembles the DMX512 stream rekordbox writes and forwards raw channel values over LAN UDP to `dmx_recv()`. See `dmx-bridge/README.md`.
- **scripts** — `follow.rhai` (the DMX-following job worth running) and `logcat.mjs` (collector for the bridge's UDP logs).

Locking: one lock at a time, `SET NX PX` + owner-checked Lua. Lock expiry is enforced *on the device* via relative TTLs, so the light returns to its idle script even if wifi dies mid-script. When nobody holds the lock the device runs an admin-editable idle script (falling back to a built-in green/yellow/red cycle).

The device holds one websocket open indefinitely; a 30s server-side ping drops half-open sockets. The firmware auto-reconnects and the server resyncs it with a `hello` message on every connect.

## API (for key holders)

```sh
BASE=https://signal.jackhogan.me
AUTH="Authorization: Bearer tl_<id>_<secret>"
JSON="Content-Type: application/json"   # required — form content types are CSRF-blocked

# Take the lock (duration capped per key; omit duration_s for your max)
curl -X POST $BASE/v1/lock -H "$AUTH" -H "$JSON" -d '{"duration_s": 120}'
# → 201 {"expiresAt": ...} | 409 {"error":"locked","holder":"amy","expiresAt":...}

# Keys minted with the override flag may steal the lock (explicit opt-in):
curl -X POST $BASE/v1/lock -H "$AUTH" -H "$JSON" -d '{"duration_s": 60, "override": true}'

# Run a script (must hold the lock; max 16KB minified, 256KB as sent; ≤20 submissions/min)
curl -X POST $BASE/v1/script -H "$AUTH" -H "$JSON" \
  -d '{"script": "loop { set_lights(true,false,false); sleep(500); set_lights(false,false,false); sleep(500); }"}'
# → 202 {"jobId":..., "ttl_ms":..., "bytes":..., "raw_bytes":...}
#   422 {"error":"Unexpected ...","line":1,"col":9} | 413 over either limit

# Release early
curl -X DELETE $BASE/v1/lock -H "$AUTH"

# Public status (no auth)
curl $BASE/v1/status

# Live status stream (no auth): a websocket that pushes the same JSON shape
# as /v1/status on every change. Reconnect on close (redeploys drop it).
#   wss://signal.jackhogan.me/api/live
```

### Script environment

Comment and indent freely. `POST /v1/script` minifies with
[rhaiper](https://crates.io/crates/rhaiper) before storing, so the 16KB limit measures the
stripped text and the device never receives a comment — `scripts/follow.rhai` goes 8251 → 5054
bytes on whitespace alone. The source map stays server-side, so runtime errors in the dashboard
still cite the line you wrote rather than a position in text nobody has seen.

Scripts are Rhai with `i64` integers and `f32` floats (no closures/modules/eval). Floats are
single-precision deliberately: rhai's default `FLOAT` is `f64`, which the ESP32's
single-precision FPU would have to emulate in software. Convert with `to_float()` — there is
no `as float` cast. These functions are available:

| fn | effect |
|---|---|
| `set_lights(r, y, g)` | set the three lamps (bools) |
| `sleep(ms)` | pause; also how your script yields |
| `sleep_until(ms)` | pause until `millis()` reaches an absolute target |
| `millis()` | ms since your script started |
| `lamp_dwell_ms()` | the configured minimum relay dwell |
| `rand_float()` | `[0.0, 1.0)` |
| `rand_int(lo, hi)` | integer in `lo..=hi` |
| `rand_chance(p)` | true with probability `p` |
| `dmx_recv(timeout_ms)` | `#{ ok, base, seq, ch }` — newest DMX frame from the bridge, or `ok: false` on timeout |

**There is no operation cap.** A run is bounded by your lock's TTL and by the kill switch,
nothing else, so a long analysis loop is fine. A busy loop with no `sleep` will still hold
the light for the whole lock, so include one.

Prefer `sleep_until` over `sleep` for anything rhythmic. A pattern built from relative sleeps
adds on every delay the work between them cost, so its period drifts long and it slides off
the beat; against an absolute target the error cannot accumulate.

`rand_*` come from the ESP32's hardware RNG. Without them a generated pattern repeats
identically every run, which is most of what makes it look mechanical — rhai ships no RNG and
there is no clock to improvise one from.

The lamps are driven by mechanical relays, so `set_lights` enforces a minimum dwell per
lamp (`min_lamp_dwell_ms`, default 100ms — about 10Hz), which `lamp_dwell_ms()` reports.
Asking for changes faster than that does not drop them: the call blocks until the relay may
move, so a strobe script runs at the cap rather than doing something you didn't write.
Blocked calls still honor your lock expiry, and apply their state on the way out.

If your script also reads DMX, gate the writes on `lamp_dwell_ms()` yourself rather than
letting `set_lights` block — a blocked call stalls your loop, and a stalled loop misses
frames. Above roughly 10Hz a relay can't mechanically follow anyway (operate
plus release is around 15ms), and its timing variance is the floor on how tight any
animation can be.

`dmx_recv` receives DMX channel values forwarded by the **dmx-bridge** (see `dmx-bridge/`),
which presents itself to rekordbox as an Enttec DMX interface. It binds a UDP socket
(`dmx_port`, default 49500) on first call and drops it when your script ends, so the port
is only open while a script is asking for it. Each call drains everything queued and
returns only the newest frame — bursts are coalesced, never replayed. It blocks like
`sleep`, so lock expiry and the kill switch still land within 10ms.

The values are **raw**, deliberately: thresholding and the channel-to-lamp mapping are
yours to decide, so they can change without reflashing anything.

| field | meaning |
|---|---|
| `ok` | a frame arrived within the timeout |
| `ch` | array of raw 0-255 channel values; `ch[0]` is channel `base` |
| `base` | DMX channel number `ch[0]` holds, so you can locate a fixture without knowing how the bridge is configured |
| `seq` | sender's frame counter; gaps mean dropped datagrams |

On timeout `ok` is false and `ch` is **empty**, so a script that ignores `ok` gets an index
error rather than quietly acting on an all-zero frame. If the socket cannot bind at all the
call raises a script error instead of timing out forever, since a silent no-op looks
identical to an idle sender.

Patched as a 3-channel RGB fixture, channel 1 is red, 2 green, 3 blue — so blue drives the
yellow lamp:

```rhai
loop {
    let p = dmx_recv(50);
    if p.ok { set_lights(p.ch[0] >= 128, p.ch[2] >= 128, p.ch[1] >= 128); }
}
```

That literal version reads as "off most of the time" in practice — rekordbox drives the fixture
only ~28% of a set, and a fixed 128 cut discards nearly half of that. `scripts/follow.rhai` is
the version worth actually running; see `scripts/README.md` for the measurements behind it.

The script is killed when your lock expires (blocking calls wake every 10ms to check) and its last `set_lights` is applied on the way out. Runtime errors (wrong arity, unknown function) surface in the dashboard history *and* on the light itself as the fault signal below; only parse errors are caught at `POST /v1/script`.

The **idle script** (admin-set, runs when nobody holds a lock) has different semantics: it runs **once per idle transition** — a one-shot script sets a state and the lamps hold it; write your own `loop { … }` for an animation. If it errors, the fault signal shows for 10s and the built-in cycle takes over.

Idle scripts (and only idle scripts) also get `get_last_holder()`, returning `#{ name, result, ended_ms_ago }` for the most recent job (`name: ""`, `ended_ms_ago: -1` if nobody has held the light since boot):

```rhai
let h = get_last_holder();
if h.result == "error" {
    // shame blink for whoever crashed their script
    loop { set_lights(true, false, false); sleep(300); set_lights(false, false, false); sleep(300); }
}
```

### The fault signal

All three lamps together at 1Hz — a combination a real signal never shows. Raised when a script errors (for 10s, then normal idle resumes) or when the link to the server has been down for over a minute, and driven by the firmware rather than a script so it still appears when the script thread is dead or could not be spawned. It is *not* raised while a job is running: DMX arrives over the LAN, so a script can be mid-show with the server unreachable.

With `min_lamp_dwell_ms = 0` (solid-state relays) it pulses at 1Hz indefinitely. With mechanical relays it eases to a short blink after 30s, because 1Hz is 7,200 transitions/lamp/hour and that would spend a real fraction of the contacts' rated life announcing a fault.

## Development

```sh
docker compose -f docker-compose.dev.yml up -d   # redis + Upstash-compatible REST proxy
nix-shell                                        # node + pnpm (JS tooling lives here)
pnpm install && pnpm -r build

# terminal 1: web app
cd apps/web && cp .env.example .env && pnpm dev
# terminal 2: device websocket server
cd apps/web && DEVICE_TOKEN=dev-device-token REDIS_URL=redis://localhost:6379 pnpm exec tsx api/device.ts
# terminal 3: live-dashboard websocket server
cd apps/web && REDIS_URL=redis://localhost:6379 pnpm exec tsx api/live.ts
# terminal 4: fake traffic light (no hardware needed)
cd apps/web && pnpm exec tsx scripts/fake-device.ts

pnpm exec tsx scripts/seed-dev.ts amy            # mint a dev API key (prints token)
pnpm exec tsx scripts/hash-password.ts <pw>      # value for ADMIN_PASSWORD_HASH
cd apps/web && pnpm exec vitest run              # lock/key integration tests
cargo test -p script-env                         # sandbox tests
pnpm run build:wasm                              # rebuild validator (commit pkg/)
```

## Deploying

Self-hosted on one Ubuntu box: docker for the three app processes, nginx for
TLS, Redis in its own compose project alongside.

- [deploy/home-redis/](deploy/home-redis/) — Redis + an Upstash-compatible REST proxy
- [deploy/home-app/](deploy/home-app/) — the app itself, nginx vhost, and the deploy steps

## Firmware

```sh
cd firmware
cp cfg.toml.example cfg.toml   # wifi creds, ws url, device token
cargo build                    # first build downloads ESP-IDF
cargo run                      # flashes + monitors via espflash
```

Wiring: relays on GPIO 32 (red), 33 (yellow), 25 (green) — non-strap pins, driven low at boot; set `active_low = true` in cfg.toml if your relay board switches on LOW.

### Relay wear budget

The board is an ESP32-WROOM-32E carrier with four **Songle SRD-05VDC-SL-C** relays. Confirm
against your own copy of the datasheet, but the figures that matter are:

| | |
|---|---|
| mechanical endurance | 10<sup>7</sup> operations |
| electrical endurance | 10<sup>5</sup> operations at rated load (10A 250VAC) |
| max switching rate | 300 operations/min mechanical, **30/min electrical** |
| operate / release | ≤10ms / ≤5ms |

`scripts/follow.rhai` measures at **216 / 90 / 240 operations per minute** for red / yellow /
green (`cargo test -p script-env --test follow -- --nocapture relay`). Against those figures:

- **~700 hours** of running against mechanical endurance.
- **~7 hours** against electrical endurance *at rated load* — but the lamps draw a small
  fraction of 10A, so the true figure is far higher. How much higher depends on inrush, not
  on steady current: these are AC LED modules with capacitive-input drivers, and the contacts
  close at a random point in the AC cycle because there is no zero-cross switching. Inrush at
  the wrong phase angle is what erodes contacts.
- **8× over the 30/min electrical switching guidance**, which is the figure a denser pattern
  makes worse.

`min_lamp_dwell_ms` is the single knob: it caps transitions at `1000 / dwell` per second per
lamp, so raising it from 100ms trades density for contact life linearly. Solid-state relays with
zero-cross switching remove the constraint entirely and let `min_lamp_dwell_ms` go to 0, which
also makes the fault signal a continuous 1Hz.

The `ops` counters in device telemetry are the odometer for this, but they reset on boot — they
measure a session, not a lifetime.
