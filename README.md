# traffic-light

A real three-lamp traffic signal on the internet. Friends, apps, and trinkets get API keys; a key can hold a time-limited **lock** on the light and submit a [Rhai](https://rhai.rs) script that the ESP32 executes. A public dashboard shows who holds the lock and what the lamps are doing.

## Architecture

```
friend ──HTTP──▶ SvelteKit /v1/* (Vercel) ──▶ Upstash Redis ◀── pub/sub ── api/device.ts (Vercel WS fn)
                    │                                                        ▲ websocket
                    └─ validates scripts via Rhai→WASM                       └── ESP32 (runs Rhai on-device)
```

- **apps/web** — SvelteKit app: public dashboard, admin panel, `/v1` JSON API, and the standalone websocket function `api/device.ts` (SvelteKit can't upgrade websockets on Vercel; the root `api/` directory can — which is also why the JSON API lives at `/v1`, not `/api`).
- **crates/script-env** — the single source of truth for the Rhai language surface and sandbox limits, used by both the validator and the firmware.
- **crates/validator** → **packages/validator-wasm** — `Engine::compile` compiled to WASM; `POST /v1/script` rejects scripts that won't parse, with line/col errors.
- **packages/protocol** — zod schemas + constants for the wire protocol and Redis keys.
- **firmware** — Rust (esp-idf) for an ESP32-WROOM-32E: wifi → SNTP → websocket, Rhai engine in a dedicated thread, three relay GPIOs (25/26/27 = R/Y/G).

Locking: one lock at a time, `SET NX PX` + owner-checked Lua. Lock expiry is enforced *on the device* via relative TTLs, so the light returns to its idle script even if wifi dies mid-script. When nobody holds the lock the device runs an admin-editable idle script (falling back to a built-in green/yellow/red cycle).

The device connection drops every ≤300s (Vercel Hobby function duration cap) — the firmware auto-reconnects and the server resyncs it with a `hello` message on every connect.

## API (for key holders)

```sh
BASE=https://<your-app>.vercel.app
AUTH="Authorization: Bearer tl_<id>_<secret>"

# Take the lock (duration capped per key; omit duration_s for your max)
curl -X POST $BASE/v1/lock -H "$AUTH" -d '{"duration_s": 120}'
# → 201 {"expiresAt": ...} | 409 {"error":"locked","holder":"amy","expiresAt":...}

# Keys minted with the override flag may steal the lock (explicit opt-in):
curl -X POST $BASE/v1/lock -H "$AUTH" -d '{"duration_s": 60, "override": true}'

# Run a script (must hold the lock; max 16KB; ≤20 submissions/min)
curl -X POST $BASE/v1/script -H "$AUTH" \
  -d '{"script": "loop { set_lights(true,false,false); sleep(500); set_lights(false,false,false); sleep(500); }"}'
# → 202 {"jobId":..., "ttl_ms":...} | 422 {"error":"Unexpected ...","line":1,"col":9}

# Release early
curl -X DELETE $BASE/v1/lock -H "$AUTH"

# Public status (no auth)
curl $BASE/v1/status
```

### Script environment

Scripts are Rhai with integers only (no floats/maps/modules/eval) and these functions:

| fn | effect |
|---|---|
| `set_lights(r, y, g)` | set the three lamps (bools) |
| `sleep(ms)` | pause; also how your script yields |
| `millis()` | ms since your script started |

The script is killed when your lock expires (`sleep` wakes every 50ms to check). Busy loops without `sleep` die early against the 5M-operation cap — use `sleep`. Runtime errors (wrong arity, unknown function) surface in the dashboard history, not at submit time; only parse errors are caught at `POST /v1/script`.

## Development

```sh
docker compose -f docker-compose.dev.yml up -d   # redis + Upstash-compatible REST proxy
nix-shell                                        # node + pnpm (JS tooling lives here)
pnpm install && pnpm -r build

# terminal 1: web app
cd apps/web && cp .env.example .env && pnpm dev
# terminal 2: device websocket function
cd apps/web && DEVICE_TOKEN=dev-device-token REDIS_URL=redis://localhost:6379 pnpm exec tsx api/device.ts
# terminal 3: fake traffic light (no hardware needed)
cd apps/web && pnpm exec tsx scripts/fake-device.ts

pnpm exec tsx scripts/seed-dev.ts amy            # mint a dev API key (prints token)
pnpm exec tsx scripts/hash-password.ts <pw>      # value for ADMIN_PASSWORD_HASH
cd apps/web && pnpm exec vitest run              # lock/key integration tests
cargo test -p script-env                         # sandbox tests
pnpm run build:wasm                              # rebuild validator (commit pkg/)
```

## Deploying (Vercel Hobby)

1. Create the Vercel project with **Root Directory = `apps/web`** (Fluid compute on, default). WebSocket support is public beta and required.
2. Add **Upstash Redis** from the Vercel Marketplace (free tier). The dashboard/API use the REST vars; the websocket function needs the **TCP** URL.
3. Environment variables:
   - `UPSTASH_REDIS_REST_URL`, `UPSTASH_REDIS_REST_TOKEN` (from the marketplace integration)
   - `REDIS_URL` — the `rediss://…` TCP connection string
   - `DEVICE_TOKEN` — long random string the ESP32 presents
   - `ADMIN_PASSWORD_HASH` — from `scripts/hash-password.ts`
   - `SESSION_SECRET` — long random string
4. Redis budget note: the status endpoint is CDN-cached 10s and the dashboard pauses polling in hidden tabs — that keeps 24/7 operation around ~165K of the 500K free commands/month. Don't add fast polling.

## Firmware

```sh
cd firmware
cp cfg.toml.example cfg.toml   # wifi creds, ws url, device token
cargo build                    # first build downloads ESP-IDF
cargo run                      # flashes + monitors via espflash
```

Wiring: relays on GPIO 25 (red), 26 (yellow), 27 (green) — non-strap pins, driven low at boot; set `active_low = true` in cfg.toml if your relay board switches on LOW.
