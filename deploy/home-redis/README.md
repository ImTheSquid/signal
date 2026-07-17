# Self-hosted Redis (Ubuntu + nginx) instead of Upstash

Same stack the dev environment uses: Redis + [SRH](https://github.com/hiett/serverless-redis-http)
(an Upstash-compatible REST proxy). The app code is unchanged — only env vars differ.

Two things get exposed through nginx:

| what | for | how |
|---|---|---|
| `https://redis.example.com` | SvelteKit routes (`@upstash/redis`, REST) | nginx → SRH on :8079 |
| `rediss://redis.example.com:6380` | WS function (`ioredis`, pub/sub) | nginx `stream` TLS → redis on :6379 |

Redis and SRH bind to 127.0.0.1 only; nothing reaches Redis without TLS + the password/token.

## On the server

```sh
# 0. DNS: A record redis.example.com → your home IP; router forwards 443 + 6380.
sudo apt install libnginx-mod-stream          # stream module for the TCP side
sudo certbot certonly --nginx -d redis.example.com

# 1. Containers
mkdir -p ~/traffic-light-redis && cd ~/traffic-light-redis
# copy docker-compose.yml here, then:
cp .env.example .env    # fill with: openssl rand -hex 24  (one per var)
docker compose up -d

# 2. nginx REST vhost (replace redis.example.com in the file first)
sudo cp nginx-rest.conf /etc/nginx/sites-available/redis-rest
sudo ln -s /etc/nginx/sites-available/redis-rest /etc/nginx/sites-enabled/

# 3. nginx TCP stream (see comments in nginx-stream.conf — the include
#    goes at nginx.conf TOP LEVEL, not inside http{})
sudo mkdir -p /etc/nginx/streams.d
sudo cp nginx-stream.conf /etc/nginx/streams.d/redis.conf
echo 'stream { include /etc/nginx/streams.d/*.conf; }' | sudo tee -a /etc/nginx/nginx.conf

sudo nginx -t && sudo systemctl reload nginx
```

## Verify from anywhere

```sh
curl -s https://redis.example.com -H "Authorization: Bearer $SRH_TOKEN" \
  -H "Content-Type: application/json" -d '["PING"]'          # {"result":"PONG"}
redis-cli --tls -h redis.example.com -p 6380 -a "$REDIS_PASSWORD" ping   # PONG
```

## Point Vercel at it

```sh
vercel env add UPSTASH_REDIS_REST_URL production    # https://redis.example.com
vercel env add UPSTASH_REDIS_REST_TOKEN production  # the SRH_TOKEN value
vercel env add REDIS_URL production                 # rediss://:<REDIS_PASSWORD>@redis.example.com:6380
vercel deploy
```

Certbot renewals: nginx reload on renew re-reads the certs for both listeners
automatically (the deploy hook that ships with certbot's nginx plugin handles it).

Notes:
- The Upstash 500K commands/month budget concern disappears; the CDN caching on
  /v1/status stays anyway (it's still good manners to your uplink).
- `noeviction` + 64MB is deliberate: this dataset is a few KB; if Redis ever
  hits that limit something is wrong, and evicting a live lock would be worse
  than erroring.
