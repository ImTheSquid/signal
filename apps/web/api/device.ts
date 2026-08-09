/**
 * Device websocket endpoint, run as a standalone Node server (SvelteKit routes
 * cannot handle WS upgrades). The ESP32 holds one connection open indefinitely
 * and resyncs via `hello` on every connect. Everything durable lives in Redis,
 * so a restart is stateless-safe. API routes signal us via the `events`
 * pub/sub channel.
 */
import http from 'node:http';
import { timingSafeEqual } from 'node:crypto';
import { Redis } from 'ioredis';
import { WebSocketServer, WebSocket } from 'ws';
import {
	COLLAPSE_HISTORY_LUA,
	DEVICE_KEY_TTL_MS,
	DEVICE_WRITE_MIN_INTERVAL_MS,
	DeviceMsgSchema,
	EventSchema,
	HISTORY_LENGTH,
	IdleSchema,
	REDIS,
	WS_MAX_PAYLOAD,
	type DeviceState,
	type HistoryEntry,
	type Idle,
	type Job,
	type ServerMsg
} from '@traffic-light/protocol';
// Relative, not `$lib`: that alias only resolves inside SvelteKit, and this runs
// as a plain Node server. tsx loads the TypeScript source directly.
import { buildHelloFrame, buildIdleFrame, buildJobFrame } from '../src/lib/server/frames.js';
// Imported straight from the wasm package rather than through `$lib`, which only
// resolves inside SvelteKit. The nodejs-target build is CJS, so plain Node loads it.
import { remap as remapError } from '@traffic-light/validator-wasm';

const redisUrl = process.env.REDIS_URL ?? 'redis://localhost:6379';
// maxRetriesPerRequest: null so commands queue rather than fail across a
// reconnect on this long-lived connection.
const redis = new Redis(redisUrl, { maxRetriesPerRequest: null, lazyConnect: true });
const subscriber = redis.duplicate();

/** Ping cadence; a device that misses one round trip is treated as gone. */
const HEARTBEAT_MS = 30_000;

/** The single connected traffic light; a new connection supersedes the old. */
let device: WebSocket | null = null;
let deviceAlive = false;
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
	// Binary, not text. esp-idf-svc validates UTF-8 per receive-buffer chunk on a
	// text frame and drops the chunk if a multi-byte character straddles the
	// boundary, which voids the whole message — one em dash in a script comment is
	// enough. Binary frames are handed to the firmware as raw bytes and validated
	// once over the reassembled document.
	if (device?.readyState === WebSocket.OPEN) device.send(Buffer.from(JSON.stringify(msg)));
}

async function currentJob(): Promise<Job | null> {
	const raw = await redis.get(REDIS.jobCurrent);
	return raw ? (JSON.parse(raw) as Job) : null;
}

async function getIdle(): Promise<Idle | null> {
	const raw = await redis.get(REDIS.idle);
	if (!raw) return null;
	// Parsed rather than cast. The record is spread straight into a frame, so
	// zod dropping unknown keys is what keeps a server-only field from riding to
	// the device. An unreadable record means the built-in cycle, which is the
	// conservative answer.
	const parsed = IdleSchema.safeParse(JSON.parse(raw));
	if (!parsed.success) {
		console.warn('idle record does not parse; the light will use its built-in cycle');
		return null;
	}
	return parsed.data;
}

async function sendHello(): Promise<void> {
	const [job, idle] = await Promise.all([currentJob(), getIdle()]);
	send(buildHelloFrame(job, idle, Date.now()));
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
	// Read before the delete below: the device reports positions against the
	// minified script, and the map that translates them expires with the job.
	const raw = await redis.get(REDIS.jobCurrent);
	const stored = raw ? (JSON.parse(raw) as Job) : null;
	// Only the job this message is about can explain its positions.
	const job = stored?.jobId === msg.id ? stored : null;

	const entries = await redis.lrange(REDIS.history, 0, HISTORY_LENGTH - 1);
	for (let i = 0; i < entries.length; i++) {
		const entry = JSON.parse(entries[i]) as HistoryEntry;
		if (entry.jobId === msg.id) {
			entry.end = Date.now();
			entry.result = msg.result;
			if (msg.error) {
				const mapped = job?.map ? remapError(job.map, job.script, msg.error) : msg.error;
				entry.error = mapped;
				// Keep what the device actually said when it differs, so a wrong map
				// cannot destroy the only copy of the error.
				if (mapped !== msg.error) entry.deviceError = msg.error;
			}
			await redis.lset(REDIS.history, i, JSON.stringify(entry));
			// Now terminal — merge into a same-key streak if adjacent.
			await redis.eval(COLLAPSE_HISTORY_LUA, 1, REDIS.history);
			break;
		}
	}
	// Clear the stored job so reconnect hellos don't replay a finished script.
	if (job) await redis.del(REDIS.jobCurrent);
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
				const job = await currentJob();
				const msg = job === null ? null : buildJobFrame(job, Date.now());
				if (msg) send(msg);
			} else if (event.type === 'abort') {
				send({ t: 'abort' });
			} else if (event.type === 'reboot') {
				send({ t: 'reboot' });
			} else if (event.type === 'idle') {
				const idle = await getIdle();
				if (idle) send(buildIdleFrame(idle));
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
	deviceAlive = true;
	ws.on('pong', () => {
		if (device === ws) deviceAlive = true;
	});

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
				heap_block: msg.heap_block,
				ops: msg.ops,
				fw: msg.fw,
				idle: msg.idle,
				idle_error: msg.idle_error
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

// Wifi can vanish without a FIN, leaving a half-open socket that silently
// swallows jobs; the ping is what surfaces that.
setInterval(() => {
	if (!device) return;
	if (!deviceAlive) {
		device.terminate(); // 'close' clears `device`
		return;
	}
	deviceAlive = false;
	device.ping();
}, HEARTBEAT_MS).unref();

const port = Number(process.env.DEVICE_WS_PORT ?? 3001);
server.listen(port, () => console.log(`device ws listening on :${port}`));
