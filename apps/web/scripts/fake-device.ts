/**
 * Protocol-conformant fake traffic light for testing without hardware:
 *   pnpm exec tsx scripts/fake-device.ts [ws://localhost:3001]
 * It can't run Rhai — a delivered job turns the "red lamp" on, then reports
 * job_done(deadline) when the ttl elapses. Reconnects like the firmware.
 */
import WebSocket from 'ws';
import {
	DEVICE_HEARTBEAT_MS,
	ServerMsgSchema,
	type DeviceMsg,
	type Lights
} from '@traffic-light/protocol';

const url = process.argv[2] ?? 'ws://localhost:3001';
const token = process.env.DEVICE_TOKEN ?? 'dev-device-token';

let lights: Lights = { r: false, y: false, g: false };
let running = 'idle';
let jobTimer: NodeJS.Timeout | null = null;
let reconnectDelay = 1000;
// Messages always go out on the live connection — a job may outlive the
// socket it arrived on (a redeploy or dropped ping reconnects mid-job).
let currentWs: WebSocket | null = null;

function log(...args: unknown[]) {
	const lamp = (on: boolean, glyph: string) => (on ? glyph : '·');
	console.log(
		`[${lamp(lights.r, '🔴')}${lamp(lights.y, '🟡')}${lamp(lights.g, '🟢')} ${running}]`,
		...args
	);
}

function sendMsg(msg: DeviceMsg) {
	if (currentWs?.readyState === WebSocket.OPEN) currentWs.send(JSON.stringify(msg));
}

function sendState() {
	sendMsg({ t: 'state', lights, running, heap: 200_000, fw: 'fake-0.1.0' });
}

function finishJob(result: 'ok' | 'error' | 'aborted' | 'deadline', error?: string) {
	if (running === 'idle') return;
	const id = running;
	running = 'idle';
	lights = { r: false, y: false, g: false };
	if (jobTimer) clearTimeout(jobTimer);
	jobTimer = null;
	sendMsg({ t: 'job_done', id, result, ...(error ? { error } : {}) });
	sendState();
	log(`job ${id} finished: ${result}`);
}

function startJob(job: { id: string; script: string; ttl_ms: number }) {
	if (running === job.id) return; // dedupe — two instances may forward the same event
	if (running !== 'idle') finishJob('aborted');
	running = job.id;
	lights = { r: true, y: false, g: false };
	jobTimer = setTimeout(() => finishJob('deadline'), job.ttl_ms);
	sendState();
	log(`started job ${job.id} (ttl ${job.ttl_ms}ms), script: ${JSON.stringify(job.script)}`);
}

function connect() {
	const ws = new WebSocket(url, { headers: { authorization: `Bearer ${token}` } });
	let heartbeat: NodeJS.Timeout;

	ws.on('open', () => {
		currentWs = ws;
		reconnectDelay = 1000;
		log(`connected to ${url}`);
		sendState();
		heartbeat = setInterval(sendState, DEVICE_HEARTBEAT_MS);
	});

	ws.on('message', (data) => {
		const parsed = ServerMsgSchema.safeParse(JSON.parse(data.toString()));
		if (!parsed.success) return log('unparseable server msg', data.toString());
		const msg = parsed.data;
		if (msg.t === 'hello') {
			log(`hello: job=${msg.job?.id ?? 'none'} idle rev=${msg.idle?.rev ?? 'none'}`);
			if (msg.job) startJob(msg.job);
			else if (running !== 'idle') finishJob('aborted');
		} else if (msg.t === 'job') {
			startJob(msg);
		} else if (msg.t === 'abort') {
			finishJob('aborted');
		} else if (msg.t === 'idle') {
			log(`idle script updated to rev ${msg.rev}`);
		}
	});

	ws.on('close', (code, reason) => {
		clearInterval(heartbeat);
		if (currentWs === ws) currentWs = null;
		log(`disconnected (${code} ${reason}), reconnecting in ${reconnectDelay}ms`);
		setTimeout(connect, reconnectDelay);
		reconnectDelay = Math.min(reconnectDelay * 2, 30_000);
	});

	ws.on('error', (err) => log('ws error:', err.message));
}

connect();
