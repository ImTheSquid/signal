/**
 * Device websocket endpoint, deployed as a standalone Vercel Function
 * (SvelteKit routes cannot handle WS upgrades). The ESP32 keeps one
 * connection open; Vercel closes it at maxDuration (300s on Hobby) and the
 * device reconnects. Everything durable lives in Redis — instance recycling
 * is stateless-safe. API routes signal us via the `events` pub/sub channel.
 */
import http from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { Redis } from 'ioredis';
import { WebSocketServer, WebSocket } from 'ws';
import {
	DEVICE_KEY_TTL_MS,
	DEVICE_WRITE_MIN_INTERVAL_MS,
	DeviceMsgSchema,
	EventSchema,
	HISTORY_LENGTH,
	REDIS,
	WS_MAX_PAYLOAD,
	type DeviceState,
	type HistoryEntry,
	type Idle,
	type Job,
	type ServerMsg
} from '@traffic-light/protocol';

const redisUrl = process.env.REDIS_URL ?? 'redis://localhost:6379';
// maxRetriesPerRequest: null per Vercel's guidance for long-lived connections.
const redis = new Redis(redisUrl, { maxRetriesPerRequest: null, lazyConnect: true });
const subscriber = redis.duplicate();

/** The single connected traffic light; a new connection supersedes the old. */
let device: WebSocket | null = null;
let lastDeviceWrite = 0;
let lastWritten = '';

function authorized(req: http.IncomingMessage): boolean {
	const token = process.env.DEVICE_TOKEN;
	const presented = req.headers.authorization?.match(/^Bearer\s+(\S+)$/)?.[1];
	if (!token || !presented) return false;
	const a = Buffer.from(token);
	const b = Buffer.from(presented);
	return a.length === b.length && timingSafeEqual(a, b);
}

function send(msg: ServerMsg): void {
	if (device?.readyState === WebSocket.OPEN) device.send(JSON.stringify(msg));
}

async function currentJobMsg(): Promise<ServerMsg | null> {
	const raw = await redis.get(REDIS.jobCurrent);
	if (!raw) return null;
	const job = JSON.parse(raw) as Job;
	const ttl = job.expiresAt - Date.now();
	if (ttl <= 0) return null;
	return { t: 'job', id: job.jobId, script: job.script, ttl_ms: ttl };
}

async function getIdle(): Promise<Idle | null> {
	const raw = await redis.get(REDIS.idle);
	return raw ? (JSON.parse(raw) as Idle) : null;
}

async function sendHello(): Promise<void> {
	const [job, idle] = await Promise.all([currentJobMsg(), getIdle()]);
	send({
		t: 'hello',
		job: job && job.t === 'job' ? { id: job.id, script: job.script, ttl_ms: job.ttl_ms } : null,
		idle
	});
}

async function writeDeviceState(state: Omit<DeviceState, 'ts'>): Promise<void> {
	const fingerprint = JSON.stringify([state.lights, state.running]);
	const changed = fingerprint !== lastWritten;
	if (!changed && Date.now() - lastDeviceWrite < DEVICE_WRITE_MIN_INTERVAL_MS) return;

	const full: DeviceState = { ...state, ts: Date.now() };
	await redis.set(REDIS.device, JSON.stringify(full), 'PX', DEVICE_KEY_TTL_MS);
	lastDeviceWrite = Date.now();
	lastWritten = fingerprint;
	await redis.publish(REDIS.eventsChannel, JSON.stringify({ type: 'update' }));
}

async function recordJobDone(msg: {
	id: string;
	result: 'ok' | 'error' | 'aborted' | 'deadline';
	error?: string;
}): Promise<void> {
	const entries = await redis.lrange(REDIS.history, 0, HISTORY_LENGTH - 1);
	for (let i = 0; i < entries.length; i++) {
		const entry = JSON.parse(entries[i]) as HistoryEntry;
		if (entry.jobId === msg.id) {
			entry.end = Date.now();
			entry.result = msg.result;
			if (msg.error) entry.error = msg.error;
			await redis.lset(REDIS.history, i, JSON.stringify(entry));
			break;
		}
	}
	// Clear the stored job so reconnect hellos don't replay a finished script.
	const raw = await redis.get(REDIS.jobCurrent);
	if (raw && (JSON.parse(raw) as Job).jobId === msg.id) {
		await redis.del(REDIS.jobCurrent);
	}
	await redis.publish(REDIS.eventsChannel, JSON.stringify({ type: 'update' }));
}

async function ensureSubscribed(): Promise<void> {
	if (subscriber.status === 'wait') {
		await subscriber.connect();
		await subscriber.subscribe(REDIS.eventsChannel);
		subscriber.on('message', async (_channel, raw) => {
			const parsed = EventSchema.safeParse(JSON.parse(raw));
			if (!parsed.success) return;
			const event = parsed.data;
			if (event.type === 'job') {
				const msg = await currentJobMsg();
				if (msg) send(msg);
			} else if (event.type === 'abort') {
				send({ t: 'abort' });
			} else if (event.type === 'idle') {
				const idle = await getIdle();
				if (idle) send({ t: 'idle', script: idle.script, rev: idle.rev });
			}
		});
	}
}

const server = http.createServer((_req, res) => {
	res.writeHead(426).end('websocket only');
});

const wss = new WebSocketServer({ noServer: true, maxPayload: WS_MAX_PAYLOAD });

server.on('upgrade', (req, socket, head) => {
	if (!authorized(req)) {
		socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
		socket.destroy();
		return;
	}
	wss.handleUpgrade(req, socket, head, (ws) => wss.emit('connection', ws, req));
});

wss.on('connection', async (ws) => {
	device?.close(4000, 'superseded');
	device = ws;

	ws.on('message', async (data) => {
		let parsed;
		try {
			parsed = DeviceMsgSchema.safeParse(JSON.parse(data.toString()));
		} catch {
			return;
		}
		if (!parsed.success) return;
		const msg = parsed.data;
		if (msg.t === 'state') {
			await writeDeviceState({
				lights: msg.lights,
				running: msg.running,
				heap: msg.heap,
				fw: msg.fw
			});
		} else if (msg.t === 'job_done') {
			await recordJobDone(msg);
		}
	});

	ws.on('close', () => {
		if (device === ws) device = null;
	});

	if (redis.status === 'wait') await redis.connect();
	await ensureSubscribed();
	await sendHello();
});

// Local dev: `pnpm exec tsx api/device.ts` — Vercel ignores this and uses the export.
if (!process.env.VERCEL) {
	const port = Number(process.env.DEVICE_WS_PORT ?? 3001);
	server.listen(port, () => console.log(`device ws listening on :${port}`));
}

export default server;
