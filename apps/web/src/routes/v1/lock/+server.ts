import { json } from '@sveltejs/kit';
import { REDIS } from '@traffic-light/protocol';
import { authenticate } from '$lib/server/keys';
import { acquireLock, releaseLock } from '$lib/server/locks';
import { pushHistory } from '$lib/server/history';
import { redis } from '$lib/server/redis';
import type { RequestHandler } from './$types';

export const POST: RequestHandler = async ({ request }) => {
	const r = redis();
	const key = await authenticate(r, request.headers.get('authorization'));
	if (!key) return json({ error: 'invalid API key' }, { status: 401 });

	const body = await request.json().catch(() => ({}));
	const requestedMs = Number(body.duration_s) > 0 ? Number(body.duration_s) * 1000 : key.maxLockMs;
	const durationMs = Math.min(requestedMs, key.maxLockMs);
	const wantOverride = body.override === true;

	if (wantOverride && !key.override) {
		return json({ error: 'this key cannot override locks' }, { status: 403 });
	}

	const result = await acquireLock(r, {
		keyId: key.id,
		name: key.name,
		durationMs,
		override: wantOverride
	});

	if (result.status === 'conflict') {
		return json(
			{ error: 'locked', holder: result.prev.name, expiresAt: result.prev.expiresAt },
			{ status: 409 }
		);
	}

	if (result.status === 'preempted') {
		// The preempted holder's script must stop now.
		await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'abort' }));
		await pushHistory(r, {
			keyId: result.prev.keyId,
			name: result.prev.name,
			jobId: '',
			start: Date.now(),
			end: Date.now(),
			result: 'preempted'
		});
	}

	const expiresAt = Date.now() + durationMs;
	return json({ expiresAt, preempted: result.status === 'preempted' }, { status: 201 });
};

export const DELETE: RequestHandler = async ({ request }) => {
	const r = redis();
	const key = await authenticate(r, request.headers.get('authorization'));
	if (!key) return json({ error: 'invalid API key' }, { status: 401 });

	const { status } = await releaseLock(r, key.id);
	if (status === 'notyours') return json({ error: 'lock held by another key' }, { status: 403 });
	if (status === 'none') return json({ released: false });

	await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'abort' }));
	return json({ released: true });
};
