# Self-hosting (Ubuntu + docker + nginx)

SvelteKit and both websocket servers, with nginx terminating TLS for
`signal.jackhogan.me`. Redis lives in its own compose project
([../home-redis/](../home-redis/)) so app redeploys never touch its data
volume; this stack joins that project's network and reaches it at `srh:80`
and `redis:6379`.

```
                      ┌── / ─────────▶ web       :3000  ──REST──┐
internet ──▶ nginx ───┼── /api/device ▶ device-ws :3001  ──TCP──▶ redis
             (TLS)    └── /api/live ──▶ live-ws   :3002  ──TCP──┘
```

Containers publish on `127.0.0.1` only; nginx is the sole way in.

## Deploy

`signal.jackhogan.me` is a proxied (orange-cloud) A record to this box,
Cloudflare SSL/TLS mode **Full (strict)**, port 443 forwarded. nginx serves the
existing `*.jackhogan.me` Cloudflare Origin certificate — no certbot.

```sh
git clone <repo> ~/traffic-light && cd ~/traffic-light/deploy/home-app
cp .env.example .env       # REDIS_PASSWORD + SRH_TOKEN must match home-redis/.env
docker compose up -d --build

sudo mkdir -p /var/cache/nginx/traffic-light
sudo cp nginx-cache.conf /etc/nginx/conf.d/traffic-light-cache.conf
sudo cp nginx-ws-snippet.conf /etc/nginx/snippets/traffic-light-ws.conf
sudo cp nginx-signal.conf /etc/nginx/sites-available/signal
sudo ln -s /etc/nginx/sites-available/signal /etc/nginx/sites-enabled/
sudo nginx -t && sudo systemctl reload nginx
```

`ADMIN_PASSWORD_HASH` comes from `scripts/hash-password.ts`; `SESSION_SECRET`
and `DEVICE_TOKEN` from `openssl rand -hex 32`.

Redeploy: `git pull && docker compose up -d --build`.

## Verify

```sh
curl -s  https://signal.jackhogan.me/v1/status             # JSON snapshot
curl -si https://signal.jackhogan.me/api/live   | head -1  # 426 without an upgrade
curl -si https://signal.jackhogan.me/api/device | head -1  # 426; a bad-token upgrade gets 401
```

## Gotchas

- `ORIGIN` in `docker-compose.yml` must match the public URL exactly, or
  SvelteKit's CSRF check rejects every POST.
- nginx 1.24 has no `http2` directive, and websocket upgrades need HTTP/1.1
  anyway — this vhost is 1.1 only.
- `proxy_read_timeout` is 1h; the servers' 30s ping is what drops dead peers,
  and what stops Cloudflare reaping idle sockets.
- Cloudflare won't cache `/v1/status` (no rule matches a JSON path), so the
  nginx cache is what absorbs dashboard polling.
- The origin IP is public regardless of the proxy — `db.jackhogan.me` is a
  grey-cloud record to this box for postgres. Blocking direct hits is a job for
  Authenticated Origin Pulls or a Cloudflare-range allowlist.

## Tightening Redis

Nothing outside needs Redis: remove the `redis-rest` vhost and the `streams.d`
TCP listener, and drop the port-6380 forward. `db.jackhogan.me` itself stays.
