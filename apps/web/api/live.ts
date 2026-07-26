/**
 * Public read-only websocket for the dashboard. Browsers connect here and
 * receive the same snapshot shape as GET /v1/status, pushed whenever anything
 * changes (any message on the Redis `events` channel).
 */
import http from 'node:http';
import { Redis } from 'ioredis';
import { WebSocketServer, WebSocket } from 'ws';
import {
	HISTORY_LENGTH,
	REDIS,
	SettingsSchema,
	type DeviceState,
	type HistoryEntry,
	type Lock
} from '@traffic-light/protocol';

const MAX_CLIENTS = 64;
/** Collapse event bursts (job + update + state) into one snapshot push. */
const DEBOUNCE_MS = 100;
/** Ping cadence; a client that misses one round trip is dropped. */
const HEARTBEAT_MS = 30_000;

const redisUrl = process.env.REDIS_URL ?? 'redis://localhost:6379';
const redis = new Redis(redisUrl, { maxRetriesPerRequest: null, lazyConnect: true });
const subscriber = redis.duplicate();

const parse = <T>(raw: string | null): T | null => (raw ? (JSON.parse(raw) as T) : null);

async function snapshot(): Promise<string> {
	const [rawLock, rawDevice, rawSettings, rawHistory] = await Promise.all([
		redis.get(REDIS.lock),
		redis.get(REDIS.device),
		redis.get(REDIS.settings),
		redis.lrange(REDIS.history, 0, HISTORY_LENGTH - 1)
	]);
	const lock = parse<Lock>(rawLock);
	const device = parse<DeviceState>(rawDevice);
	const settings = SettingsSchema.safeParse(parse(rawSettings) ?? {});
	const historyPublic = settings.success ? settings.data.historyPublic : true;
	const history = historyPublic
		? rawHistory.map((raw) => JSON.parse(raw) as HistoryEntry)
		: [];

	return JSON.stringify({ lock, device, online: device !== null, historyPublic, history });
}

const wss = new WebSocketServer({ noServer: true, maxPayload: 1024 });

/** Clients that answered the last heartbeat ping. */
const alive = new WeakSet<WebSocket>();

let pushTimer: NodeJS.Timeout | null = null;

function schedulePush(): void {
	if (pushTimer || wss.clients.size === 0) return;
	pushTimer = setTimeout(async () => {
		pushTimer = null;
		try {
			const payload = await snapshot();
			for (const client of wss.clients) {
				if (client.readyState === WebSocket.OPEN) client.send(payload);
			}
		} catch (e) {
			console.error('snapshot push failed:', e);
		}
	}, DEBOUNCE_MS);
}

async function ensureSubscribed(): Promise<void> {
	if (subscriber.status === 'wait') {
		await subscriber.connect();
		await subscriber.subscribe(REDIS.eventsChannel);
		// Every event type implies the public snapshot may have changed.
		subscriber.on('message', schedulePush);
	}
}

const server = http.createServer((_req, res) => {
	res.writeHead(426).end('websocket only');
});

server.on('upgrade', (req, socket, head) => {
	if (wss.clients.size >= MAX_CLIENTS) {
		socket.write('HTTP/1.1 503 Service Unavailable\r\n\r\n');
		socket.destroy();
		return;
	}
	wss.handleUpgrade(req, socket, head, (ws) => wss.emit('connection', ws, req));
});

wss.on('connection', async (ws) => {
	ws.on('error', () => ws.close());
	alive.add(ws);
	ws.on('pong', () => alive.add(ws));
	if (redis.status === 'wait') await redis.connect();
	await ensureSubscribed();
	ws.send(await snapshot());
});

// Browsers that vanish (laptop lid, dropped wifi) never send a close frame;
// without this they occupy MAX_CLIENTS slots indefinitely.
setInterval(() => {
	for (const client of wss.clients) {
		if (!alive.has(client)) {
			client.terminate();
			continue;
		}
		alive.delete(client);
		client.ping();
	}
}, HEARTBEAT_MS).unref();

const port = Number(process.env.LIVE_WS_PORT ?? 3002);
server.listen(port, () => console.log(`live ws listening on :${port}`));
