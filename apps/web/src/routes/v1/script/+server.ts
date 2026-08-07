import { json } from '@sveltejs/kit';
import { COMPONENTS, ComponentsSchema, MAX_RAW_SCRIPT_BYTES, REDIS } from '@traffic-light/protocol';
import { authenticate } from '$lib/server/keys';
import { submitJob } from '$lib/server/locks';
import { pushHistory } from '$lib/server/history';
import { redis } from '$lib/server/redis';
import { isValidationError, prepareScript } from '$lib/server/validate';
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
	if (Buffer.byteLength(script) > MAX_RAW_SCRIPT_BYTES) {
		return json({ error: `script exceeds ${MAX_RAW_SCRIPT_BYTES} bytes` }, { status: 413 });
	}

	// A declaration is the submitter's promise about what the script uses, and it
	// cannot be checked by compiling — rhai resolves calls at run time. What can
	// be checked is that every name is real, so a typo fails here instead of
	// silently dropping a component and killing the script mid-run.
	let components: string[] | undefined;
	if (body?.components !== undefined) {
		const parsed = ComponentsSchema.safeParse(body.components);
		if (!parsed.success) {
			return json(
				{ error: `components must be an array of: ${COMPONENTS.join(', ')}` },
				{ status: 400 }
			);
		}
		components = parsed.data;
	}

	// Minifies as well as compile-checks. The device limit applies to what comes
	// out, so comments and indentation no longer count against it.
	const prepared = prepareScript(script);
	if (isValidationError(prepared)) {
		return json(prepared, { status: prepared.tooBig ? 413 : 422 });
	}

	const jobId = crypto.randomUUID();
	const result = await submitJob(r, key.id, {
		jobId,
		keyId: key.id,
		holder: key.name,
		script: prepared.script,
		map: prepared.map,
		rawBytes: prepared.rawBytes,
		components,
		artifact: prepared.artifact,
		positions: prepared.positions
	});
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

	// `warning` is only set when minification was declined, and JSON drops it when
	// undefined, so the field appears exactly when there is something to say.
	return json(
		{
			jobId,
			ttl_ms: result.ttlMs,
			bytes: prepared.bytes,
			raw_bytes: prepared.rawBytes,
			// What the device loads, which is the size its limit is about.
			artifact_bytes: prepared.artifactBytes,
			// Nodes that stayed a tree and will run on rhai's walker. Reported
			// rather than rejected: it is a cost, and the submitter is the one
			// who can do something about it.
			residual: prepared.residual,
			components,
			warning: prepared.warning
		},
		{ status: 202 }
	);
};
