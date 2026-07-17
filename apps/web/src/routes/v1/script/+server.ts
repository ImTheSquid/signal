import { json } from '@sveltejs/kit';
import { MAX_SCRIPT_BYTES, REDIS } from '@traffic-light/protocol';
import { authenticate } from '$lib/server/keys';
import { submitJob } from '$lib/server/locks';
import { pushHistory } from '$lib/server/history';
import { redis } from '$lib/server/redis';
import { validateScript } from '$lib/server/validate';
import type { RequestHandler } from './$types';

const SUBMITS_PER_MINUTE = 20;

export const POST: RequestHandler = async ({ request }) => {
	const r = redis();
	const key = await authenticate(r, request.headers.get('authorization'));
	if (!key) return json({ error: 'invalid API key' }, { status: 401 });

	const count = await r.incr(REDIS.rateLimit(key.id));
	if (count === 1) await r.pexpire(REDIS.rateLimit(key.id), 60_000);
	if (count > SUBMITS_PER_MINUTE) {
		return json({ error: 'rate limit exceeded' }, { status: 429 });
	}

	const body = await request.json().catch(() => null);
	const script = body?.script;
	if (typeof script !== 'string' || script.length === 0) {
		return json({ error: 'body must be {"script": "..."}' }, { status: 400 });
	}
	if (Buffer.byteLength(script) > MAX_SCRIPT_BYTES) {
		return json({ error: `script exceeds ${MAX_SCRIPT_BYTES} bytes` }, { status: 413 });
	}

	const invalid = validateScript(script);
	if (invalid) return json(invalid, { status: 422 });

	const jobId = crypto.randomUUID();
	const result = await submitJob(r, key.id, { jobId, keyId: key.id, script });
	if (result.status !== 'ok') {
		const error =
			result.status === 'nolock' ? 'no lock held — POST /v1/lock first' : 'lock held by another key';
		return json({ error }, { status: 409 });
	}

	await r.publish(REDIS.eventsChannel, JSON.stringify({ type: 'job', jobId }));
	await pushHistory(r, {
		keyId: key.id,
		name: key.name,
		jobId,
		start: Date.now(),
		end: null,
		result: 'running'
	});

	return json({ jobId, ttl_ms: result.ttlMs }, { status: 202 });
};
