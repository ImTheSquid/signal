import { fail } from '@sveltejs/kit';
import { REDIS, type Idle, type Lock } from '@traffic-light/protocol';
import {
	SESSION_COOKIE,
	createSessionToken,
	verifyPassword,
	verifySessionToken
} from '$lib/server/auth';
import { clearHistory, getHistory, getSettings, setSettings } from '$lib/server/history';
import { createKey, revokeKey } from '$lib/server/keys';
import { forceRelease, getLock } from '$lib/server/locks';
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

export const load: PageServerLoad = async ({ cookies }) => {
	if (!authed(cookies)) {
		return {
			authed: false as const,
			keys: [],
			idle: null,
			lock: null,
			history: [],
			historyPublic: true
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

	return { authed: true as const, keys, idle: idle?.script ?? '', lock, history, historyPublic };
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

	clearHistory: async ({ cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		await clearHistory(redis());
		return { ok: true };
	},

	setHistoryVisibility: async ({ request, cookies }) => {
		if (!authed(cookies)) return fail(401, { error: 'not logged in' });
		const form = await request.formData();
		await setSettings(redis(), { historyPublic: form.get('historyPublic') === 'on' });
		return { ok: true };
	}
};
