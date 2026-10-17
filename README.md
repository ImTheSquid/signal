# traffic-light

A real three-lamp traffic signal on the internet. Friends, apps, and trinkets get API keys; a key can hold a time-limited **lock** on the light and submit a [Rhai](https://rhai.rs) script that the ESP32 executes. A public dashboard shows who holds the lock and what the lamps are doing.

## Architecture

```
friend ──HTTP──▶ SvelteKit /v1/* ──────────▶ Redis ◀────── pub/sub ────── api/device.ts
                    │                          ▲                             ▲ websocket
                    └─ validates + lowers      │ pub/sub                     └── ESP32 (runs Rhai on-device)
                       via Rhai→WASM       api/live.ts
                                               ▲ websocket
                                            dashboard (live updates)
```

Everything runs on one box behind nginx — see [deploy/home-app/](deploy/home-app/).

- **apps/web** — SvelteKit app: public dashboard (live over websocket, 10s polling as fallback), admin panel (keys, idle script editor, lamp test, history controls, kill switch), the `/v1` JSON API, and two standalone websocket servers: `api/device.ts` (the ESP32) and `api/live.ts` (browsers). SvelteKit routes can't upgrade websockets, so those run as their own processes — which is also why the JSON API lives at `/v1`, not `/api`.
- **crates/script-env** — the single source of truth for the Rhai language surface and sandbox limits, used by both the validator and the firmware.
- **crates/validator** → **packages/validator-wasm** — `Engine::compile` plus lowering to the device artifact, compiled to WASM. `POST /v1/script` rejects scripts that won't parse, with line/col errors, and stores the rest as submitted alongside their artifact.
- **packages/protocol** — zod schemas + constants for the wire protocol and Redis keys.
- **firmware** — Rust (esp-idf) for an ESP32-WROOM-32E: wifi → SNTP → websocket, Rhai engine in a dedicated thread, three relay GPIOs (32/33/25 = R/Y/G).
- **crates/audio-bridge** — Rust daemon on a host machine that captures the master output, estimates tempo with TempoCNN and sends the light a beat *prediction* over LAN UDP to `dmx_recv()`. Runs on macOS via a BlackHole clone, or on a Steam Deck off a Focusrite Scarlett — see `crates/audio-bridge/deck/`.
- **scripts** — `pulse.rhai` (the job worth running), `run-script.sh` (holds the lock and resubmits for a whole set) and `watch.mjs` (records what the lamps actually did).

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

# Run a script (must hold the lock; 256KB as sent; ≤20 submissions/min)
curl -X POST $BASE/v1/script -H "$AUTH" -H "$JSON" \
  -d '{"script": "loop { set_lights(true,false,false); sleep(500); set_lights(false,false,false); sleep(500); }"}'
# → 202 {"jobId":..., "ttl_ms":..., "bytes":..., "artifact_bytes":..., "residual":0}
#   422 {"error":"Unexpected ...","line":1,"col":9} | 413 script or artifact too large

# Release early
curl -X DELETE $BASE/v1/lock -H "$AUTH"

# Public status (no auth)
curl $BASE/v1/status

# Live status stream (no auth): a websocket that pushes the same JSON shape
# as /v1/status on every change. Reconnect on close (redeploys drop it).
#   wss://signal.jackhogan.me/api/live
```

### Script environment

Comment and indent freely. `POST /v1/script` compiles your script and sends the light the
lowered artifact, never the text, so comments cost the device nothing. The full Rhai standard
library is available to every script with nothing to declare: its functions are loaded lazily,
looked up the first time a script calls them, so an unused one costs no heap. The price is a
slightly slower first call to each function in a run.

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
| `dmx_recv(timeout_ms)` | `#{ ok, base, seq, ch }` — newest frame from the audio bridge, or `ok: false` on timeout |

**There is no operation cap.** A run is bounded by your lock's TTL and by the kill switch,
nothing else, so a long analysis loop is fine. A busy loop with no `sleep` will still hold
the light for the whole lock, so include one.

### What actually reaches the light

Your script is **compiled to bytecode on the server**. The parser, the optimiser and the syntax
tree all stay here; the light receives a flat artifact, verifies it and runs it. That is the
difference between roughly 756 heap allocations and 54 for `scripts/pulse.rhai` — and the
ESP32's allocator charges about 8 bytes of header on every one.

`POST /v1/script` reports `artifact_bytes` alongside `bytes`, and that is the number the device's
limit is about. It also reports `residual`: anything the compiler could not lower stays a tree
fragment and runs on rhai's walker at the old cost. It is zero for everything in this repo; if
yours is not, that part of your script is paying for an interpreter it did not need.

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

If your script also reads the beat block, gate the writes on `lamp_dwell_ms()` yourself
rather than letting `set_lights` block — a blocked call stalls your loop, and a stalled
loop misses frames. Above roughly 10Hz a relay can't mechanically follow anyway (operate
plus release is around 15ms), and its timing variance is the floor on how tight any
animation can be.

`dmx_recv` receives frames sent by the **audio-bridge** (see `crates/audio-bridge/`). It
binds a UDP socket (`dmx_port`, default 49500) on first call and drops it when your script
ends, so the port is only open while a script is asking for it. Each call drains everything
queued and returns only the newest frame — bursts are coalesced, never replayed. It blocks
like `sleep`, so lock expiry and the kill switch still land within 10ms.

The bytes are **raw**, deliberately: how they map to lamps is yours to decide, so it can
change without reflashing anything.

| field | meaning |
|---|---|
| `ok` | a frame arrived within the timeout |
| `ch` | array of raw 0-255 bytes; the audio bridge packs its beat block here |
| `base` | which sender this is; the audio bridge uses `0xFFFE`, so a script can tell senders apart |
| `seq` | sender's frame counter; gaps mean dropped datagrams |

On timeout `ok` is false and `ch` is **empty**, so a script that ignores `ok` gets an index
error rather than quietly acting on an all-zero frame. If the socket cannot bind at all the
call raises a script error instead of timing out forever, since a silent no-op looks
identical to an idle sender.

The block carries a *prediction* — "the next beat is in N ms, the period is P" — so a script
runs the grid forward itself rather than chasing an event it is always a fraction of a beat
behind. That is what makes loss cheap: at 75% packet loss the on-grid figure moves 0.951 →
0.949. `scripts/pulse.rhai` is the version worth actually running; see `scripts/README.md`
for the block layout and the measurements behind it.

The script is killed when your lock expires (blocking calls wake every 10ms to check) and its last `set_lights` is applied on the way out. Runtime errors (wrong arity, unknown function) surface in the dashboard history *and* on the light itself as the fault signal below; only parse errors are caught at `POST /v1/script`.

The **idle script** (admin-set, runs when nobody holds a lock) has different semantics: it runs **once per idle transition** — a one-shot script sets a state and the lamps hold it; write your own `loop { … }` for an animation. If it errors, the fault signal shows once and the built-in cycle takes over, and it is retried on a 30s→300s ladder rather than left broken until someone saves again.

An idle run that could not start for lack of heap is not a broken script, so it does not raise the fault; it retries on a 2s→60s ladder and reports itself as `idle: builtin` on the dashboard until it fits.

Idle scripts (and only idle scripts) also get `get_last_holder()`, returning `#{ name, result, ended_ms_ago }` for the most recent job (`name: ""`, `ended_ms_ago: -1` if nobody has held the light since boot):

```rhai
let h = get_last_holder();
if h.result == "error" {
    // shame blink for whoever crashed their script
    loop { set_lights(true, false, false); sleep(300); set_lights(false, false, false); sleep(300); }
}
```

### The fault signal

All three lamps together at 1Hz — a combination a real signal never shows. Raised when a script errors (for 10s, then normal idle resumes) or when the link to the server has been down for over a minute, and driven by the firmware rather than a script so it still appears when the script thread is dead or could not be spawned. It is *not* raised while a job is running: the beat block arrives over the LAN, so a script can be mid-show with the server unreachable.

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

`scripts/pulse.rhai` measures at **168 / 202 / 165 operations per minute** for red / yellow /
green (`cargo test -p script-env --test pulse -- --nocapture relay`). Against the busiest lamp:

- **~825 hours** of running against mechanical endurance.
- **~8 hours** against electrical endurance *at rated load* — but the lamps draw a small
  fraction of 10A, so the true figure is far higher. How much higher depends on inrush, not
  on steady current: these are AC LED modules with capacitive-input drivers, and the contacts
  close at a random point in the AC cycle because there is no zero-cross switching. Inrush at
  the wrong phase angle is what erodes contacts.
- **~7× over the 30/min electrical switching guidance**, which is the figure a denser pattern
  makes worse.

`min_lamp_dwell_ms` is the single knob: it caps transitions at `1000 / dwell` per second per
lamp, so raising it from 100ms trades density for contact life linearly. Solid-state relays with
zero-cross switching remove the constraint entirely and let `min_lamp_dwell_ms` go to 0, which
also makes the fault signal a continuous 1Hz.

The `ops` counters in device telemetry are the odometer for this, but they reset on boot — they
measure a session, not a lifetime.
