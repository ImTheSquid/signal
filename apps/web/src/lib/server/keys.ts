import { createHash, randomBytes, timingSafeEqual } from 'node:crypto';
import { REDIS } from '@traffic-light/protocol';
import type { Redis } from '@upstash/redis';

export interface ApiKey {
	id: string;
	name: string;
	maxLockMs: number;
	override: boolean;
}

interface StoredKey {
	name: string;
	secretHash: string;
	maxLockMs: number;
	override: number;
	revoked: number;
	createdAt: number;
}

function hashSecret(secret: string): string {
	return createHash('sha256').update(secret).digest('hex');
}

/** Mint a key. Returns the full bearer token — shown once, never stored. */
export async function createKey(
	r: Redis,
	opts: { name: string; maxLockMs: number; override?: boolean }
): Promise<{ key: ApiKey; token: string }> {
	const id = randomBytes(6).toString('hex');
	const secret = randomBytes(24).toString('base64url');
	const stored: StoredKey = {
		name: opts.name,
		secretHash: hashSecret(secret),
		maxLockMs: opts.maxLockMs,
		override: opts.override ? 1 : 0,
		revoked: 0,
		createdAt: Date.now()
	};
	await r.hset(REDIS.key(id), { ...stored });
	await r.sadd(REDIS.keys, id);
	return {
		key: { id, name: opts.name, maxLockMs: opts.maxLockMs, override: !!opts.override },
		token: `tl_${id}_${secret}`
	};
}

export async function revokeKey(r: Redis, id: string): Promise<void> {
	await r.hset(REDIS.key(id), { revoked: 1 });
}

/** Resolve `Authorization: Bearer tl_<id>_<secret>` to a live key, or null. */
export async function authenticate(r: Redis, authorization: string | null): Promise<ApiKey | null> {
	const token = authorization?.match(/^Bearer\s+(\S+)$/)?.[1];
	const parts = token?.match(/^tl_([0-9a-f]+)_([A-Za-z0-9_-]+)$/);
	if (!parts) return null;
	const [, id, secret] = parts;

	const stored = await r.hgetall<Record<string, unknown>>(REDIS.key(id));
	if (!stored || !stored.secretHash) return null;

	const expected = Buffer.from(String(stored.secretHash), 'hex');
	const actual = Buffer.from(hashSecret(secret), 'hex');
	if (expected.length !== actual.length || !timingSafeEqual(expected, actual)) return null;
	if (Number(stored.revoked)) return null;

	return {
		id,
		name: String(stored.name),
		maxLockMs: Number(stored.maxLockMs),
		override: !!Number(stored.override)
	};
}
