import { Redis } from '@upstash/redis';
import { beforeEach, describe, expect, it } from 'vitest';
import { authenticate, createKey } from './keys';
import { acquireLock, getLock, releaseLock, submitJob } from './locks';

// Local dev stack: `docker compose -f docker-compose.dev.yml up -d` at repo root.
const r = new Redis({
	url: process.env.UPSTASH_REDIS_REST_URL ?? 'http://localhost:8079',
	token: process.env.UPSTASH_REDIS_REST_TOKEN ?? 'dev_token'
});

const sleep = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));

beforeEach(async () => {
	await r.flushdb();
});

describe('api keys', () => {
	it('round-trips through authenticate', async () => {
		const { key, token } = await createKey(r, { name: 'amy', maxLockMs: 60_000 });
		const authed = await authenticate(r, `Bearer ${token}`);
		expect(authed).toEqual({ id: key.id, name: 'amy', maxLockMs: 60_000, override: false });
	});

	it('rejects a wrong secret and malformed tokens', async () => {
		const { key } = await createKey(r, { name: 'amy', maxLockMs: 60_000 });
		expect(await authenticate(r, `Bearer tl_${key.id}_wrongsecret`)).toBeNull();
		expect(await authenticate(r, 'Bearer garbage')).toBeNull();
		expect(await authenticate(r, null)).toBeNull();
	});
});

describe('locks', () => {
	it('acquires when free and conflicts for a second key', async () => {
		const a = await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		expect(a.status).toBe('acquired');

		const b = await acquireLock(r, { keyId: 'b', name: 'B', durationMs: 5_000, override: false });
		expect(b.status).toBe('conflict');
		if (b.status === 'conflict') expect(b.prev.name).toBe('A');
	});

	it('extends for the same key', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		const again = await acquireLock(r, {
			keyId: 'a',
			name: 'A',
			durationMs: 5_000,
			override: false
		});
		expect(again.status).toBe('extended');
	});

	it('expires on its own', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 300, override: false });
		await sleep(500);
		expect(await getLock(r)).toBeNull();
		const b = await acquireLock(r, { keyId: 'b', name: 'B', durationMs: 5_000, override: false });
		expect(b.status).toBe('acquired');
	});

	it('only the owner can release', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		expect((await releaseLock(r, 'b')).status).toBe('notyours');
		expect((await releaseLock(r, 'a')).status).toBe('released');
		expect(await getLock(r)).toBeNull();
		expect((await releaseLock(r, 'a')).status).toBe('none');
	});

	it('override preempts and clears the pending job', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		await submitJob(r, 'a', { jobId: 'job-a', keyId: 'a', holder: 'A', script: 'sleep(1)' });

		const b = await acquireLock(r, { keyId: 'b', name: 'B', durationMs: 5_000, override: true });
		expect(b.status).toBe('preempted');
		if (b.status === 'preempted') expect(b.prev.keyId).toBe('a');

		expect((await getLock(r))?.keyId).toBe('b');
		expect(await r.get('job:current')).toBeNull();
	});

	it('without override flag a held lock is a conflict even if requested', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		const b = await acquireLock(r, { keyId: 'b', name: 'B', durationMs: 5_000, override: false });
		expect(b.status).toBe('conflict');
	});
});

describe('job submission', () => {
	it('requires a lock', async () => {
		const result = await submitJob(r, 'a', { jobId: 'j', keyId: 'a', holder: 'A', script: 'sleep(1)' });
		expect(result.status).toBe('nolock');
	});

	it('rejects a non-holder', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 5_000, override: false });
		const result = await submitJob(r, 'b', { jobId: 'j', keyId: 'b', holder: 'B', script: 'sleep(1)' });
		expect(result.status).toBe('notyours');
	});

	it('clamps job TTL to the lock remaining time', async () => {
		await acquireLock(r, { keyId: 'a', name: 'A', durationMs: 2_000, override: false });
		await sleep(300);
		const result = await submitJob(r, 'a', { jobId: 'j', keyId: 'a', holder: 'A', script: 'sleep(1)' });
		expect(result.status).toBe('ok');
		if (result.status === 'ok') {
			expect(result.ttlMs).toBeGreaterThan(1_000);
			expect(result.ttlMs).toBeLessThanOrEqual(1_700);
		}
		const job = await r.get<{ expiresAt: number }>('job:current');
		expect(job?.expiresAt).toBeGreaterThan(Date.now());
		expect(job?.expiresAt).toBeLessThanOrEqual(Date.now() + 1_700);
	});
});
