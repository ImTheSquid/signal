# traffic-light

A real three-lamp traffic signal on the internet. Friends, apps, and trinkets get API keys; a key can hold a time-limited **lock** on the light and submit a [Rhai](https://rhai.rs) script that the ESP32 executes. A public dashboard shows who holds the lock and what the lamps are doing.

## Architecture

```
friend ──HTTP──▶ SvelteKit /v1/* ──────────▶ Redis ◀────── pub/sub ────── api/device.ts
                    │                          ▲                             ▲ websocket
                    └─ validates scripts       │ pub/sub                     └── ESP32 (runs Rhai on-device)
                       via Rhai→WASM       api/live.ts
                                               ▲ websocket
                                            dashboard (live updates)
```

Everything runs on one box behind nginx — see [deploy/home-app/](deploy/home-app/).

- **apps/web** — SvelteKit app: public dashboard (live over websocket, 10s polling as fallback), admin panel (keys, idle script editor, lamp test, history controls, kill switch), the `/v1` JSON API, and two standalone websocket servers: `api/device.ts` (the ESP32) and `api/live.ts` (browsers). SvelteKit routes can't upgrade websockets, so those run as their own processes — which is also why the JSON API lives at `/v1`, not `/api`.
- **crates/script-env** — the single source of truth for the Rhai language surface and sandbox limits, used by both the validator and the firmware.
- **crates/validator** → **packages/validator-wasm** — `Engine::compile` compiled to WASM; `POST /v1/script` rejects scripts that won't parse, with line/col errors.
- **packages/protocol** — zod schemas + constants for the wire protocol and Redis keys.
- **firmware** — Rust (esp-idf) for an ESP32-WROOM-32E: wifi → SNTP → websocket, Rhai engine in a dedicated thread, three relay GPIOs (32/33/25 = R/Y/G).

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

# Run a script (must hold the lock; max 16KB; ≤20 submissions/min)
curl -X POST $BASE/v1/script -H "$AUTH" -H "$JSON" \
  -d '{"script": "loop { set_lights(true,false,false); sleep(500); set_lights(false,false,false); sleep(500); }"}'
# → 202 {"jobId":..., "ttl_ms":...} | 422 {"error":"Unexpected ...","line":1,"col":9}

# Release early
curl -X DELETE $BASE/v1/lock -H "$AUTH"

# Public status (no auth)
curl $BASE/v1/status

# Live status stream (no auth): a websocket that pushes the same JSON shape
# as /v1/status on every change. Reconnect on close (redeploys drop it).
#   wss://signal.jackhogan.me/api/live
```

### Script environment

Scripts are Rhai with integers only (no floats/maps/modules/eval) and these functions:

| fn | effect |
|---|---|
| `set_lights(r, y, g)` | set the three lamps (bools) |
| `sleep(ms)` | pause; also how your script yields |
| `millis()` | ms since your script started |

The script is killed when your lock expires (`sleep` wakes every 50ms to check). Busy loops without `sleep` die early against the 5M-operation cap — use `sleep`. Runtime errors (wrong arity, unknown function) surface in the dashboard history, not at submit time; only parse errors are caught at `POST /v1/script`.

The **idle script** (admin-set, runs when nobody holds a lock) has different semantics: it runs **once per idle transition** with no operation cap — a one-shot script sets a state and the lamps hold it; write your own `loop { … }` for an animation. If it errors, the built-in cycle takes over.

Idle scripts (and only idle scripts) also get `get_last_holder()`, returning `#{ name, result, ended_ms_ago }` for the most recent job (`name: ""`, `ended_ms_ago: -1` if nobody has held the light since boot):

```rhai
let h = get_last_holder();
if h.result == "error" {
    // shame blink for whoever crashed their script
    loop { set_lights(true, false, false); sleep(300); set_lights(false, false, false); sleep(300); }
}
```

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
