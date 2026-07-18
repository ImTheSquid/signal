import { fail } from '@sveltejs/kit';
import { REDIS, type DeviceState, type Idle, type Lock } from '@traffic-light/protocol';
import {
	SESSION_COOKIE,
	createSessionToken,
	verifyPassword,
	verifySessionToken
} from '$lib/server/auth';
import { clearHistory, getHistory, getSettings, pushHistory, setSettings } from '$lib/server/history';
import { createKey, revokeKey } from '$lib/server/keys';
import { acquireLock, forceRelease, getLock, releaseLock, submitJob } from '$lib/server/locks';
import { redis } from '$lib/server/redis';
import { validateScript } from '$lib/server/validate';
import type { Actions, PageServerLoad } from './$types';

export interface AdminKeyRow {
	id: string;
	name: string;
	maxLockMs: number;
	override: boolean;
	revoked: boolean;
}

function authed(cookies: { get: (name: string) => string | undefined }): boolean {
	return verifySessionToken(cookies.get(SESSION_COOKIE));
}

// Lamp tests ride the normal lock/job pipeline under a pseudo-key (real key
// ids are hex, so no collision). The device sees an ordinary job.
const TEST_KEY_ID = 'admin:test';
const TEST_KEY_NAME = 'admin (lamp test)';
const TEST_DURATION_MS = 60_000;

export const load: PageServerLoad = async ({ cookies }) => {
	if (!authed(cookies)) {
		return {
			authed: false as const,
			keys: [],
			idle: null,
			lock: null,
			history: [],
			historyPublic: true,
			online: false
		};
	}

	const r = redis();
	const ids = await r.smembers(REDIS.keys);
	const keys: AdminKeyRow[] = await Promise.all(
		ids.map(async (id) => {
			const stored = (await r.hgetall<Record<string, unknown>>(REDIS.key(id))) ?? {};
			return {
				id,
				name: String(stored.name ?? '?'),
				maxLockMs: Number(stored.maxLockMs ?? 0),
				override: !!Number(stored.override),
				revoked: !!Number(stored.revoked)
			};
		})
	);
	const idle = await r.get<Idle>(REDIS.idle);
	const lock: Lock | null = await getLock(r);
	const history = await getHistory(r);
	const { historyPublic } = await getSettings(r);
	const device = await r.get<DeviceState>(REDIS.device);

	return {
		authed: true as const,
		keys,
		idle: idle?.script ?? '',
		lock,
		history,
		historyPublic,
		online: device !== null
	};
};

export const actions: Actions = {
	login: async ({ request, cookies }) => {
		const form = await request.formData();
		const password = String(form.get('password') ?? '');
		if (!verifyPassword(password)) return fail(401, { error: 'wrong password' });
		cookies.set(SESSION_COOKIE, createSessionToken(), {
			path: '/',
			httpOnly: true,
			sameSite: 'lax',
			secure: true,
			maxAge: 7 * 24 * 60 * 60
		});
		return { ok: true };
	},

	logout: async ({ cookies }) => {
		cookies.delete(SESSION_COOKIE, { path: '/' });
		return { ok: true };
	},

	createKey: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		const name = String(form.get('name') ?? '').trim();
		const maxLockMinutes = Number(form.get('maxLockMinutes') ?? 0);
		const override = form.get('override') === 'on';
		if (!name) return fail(400, { error: 'name is required' });
		if (!(maxLockMinutes > 0)) return fail(400, { error: 'max lock minutes must be > 0' });

		const { token } = await createKey(redis(), {
			name,
			maxLockMs: Math.round(maxLockMinutes * 60_000),
			override
		});
		// Shown once — only the hash is stored.
		return { token };
	},

	revokeKey: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		const id = String(form.get('id') ?? '');
		if (!id) return fail(400, { error: 'missing key id' });
		await revokeKey(redis(), id);
		return { ok: true };
	},

	setIdle: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		const script = String(form.get('script') ?? '');
		const invalid = validateScript(script);
		if (invalid) {
			return fail(422, { error: `line ${invalid.line ?? '?'}: ${invalid.error}` });
		}
		const r = redis();
		const prev = await r.get<Idle>(REDIS.idle);
		const idle: Idle = { script, rev: (prev?.rev ?? 0) + 1 };
		await r.set(REDIS.idle, JSON.stringify(idle));
		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'idle' }));
		return { ok: true };
	},

	kill: async ({ cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const r = redis();
		await forceRelease(r);
		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'abort' }));
		return { ok: true };
	},

	testLights: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		const [r_, y, g] = ['r', 'y', 'g'].map((lamp) => form.get(lamp) === 'on');

		const r = redis();
		const acquired = await acquireLock(r, {
			keyId: TEST_KEY_ID,
			name: TEST_KEY_NAME,
			durationMs: TEST_DURATION_MS,
			override: false
		});
		if (acquired.status === 'conflict') {
			return fail(409, {
				error: `lock held by ${acquired.prev.name} — force release it first`
			});
		}

		const script = `set_lights(${r_}, ${y}, ${g});\nsleep(${TEST_DURATION_MS});`;
		const jobId = crypto.randomUUID();
		const result = await submitJob(r, TEST_KEY_ID, { jobId, keyId: TEST_KEY_ID, script });
		if (result.status !== 'ok') return fail(500, { error: `job submit failed: ${result.status}` });

		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'job', jobId }));
		await pushHistory(r, {
			keyId: TEST_KEY_ID,
			name: TEST_KEY_NAME,
			jobId,
			start: Date.now(),
			end: null,
			result: 'running'
		});
		return { ok: true };
	},

	endTest: async ({ cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const r = redis();
		const { status } = await releaseLock(r, TEST_KEY_ID);
		if (status === 'released') {
			await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'abort' }));
		}
		return { ok: true };
	},

	clearHistory: async ({ cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const r = redis();
		await clearHistory(r);
		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'update' }));
		return { ok: true };
	},

	setHistoryVisibility: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		const r = redis();
		await setSettings(r, { historyPublic: form.get('historyPublic') === 'on' });
		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'update' }));
		return { ok: true };
	}
};
