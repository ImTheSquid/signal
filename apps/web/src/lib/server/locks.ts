import { REDIS, type Job, type Lock } from '@traffic-light/protocol';
import type { Redis } from '@upstash/redis';

/**
 * Lock operations. Anything that checks the current holder before acting runs
 * as a Redis-side Lua script so the check and the write are atomic — a plain
 * GET-then-SET/DEL races with lock expiry.
 */

const ACQUIRE = `
local cur = redis.call('GET', KEYS[1])
if not cur then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
  return {'acquired', ''}
end
if cjson.decode(cur).keyId == ARGV[3] then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
  return {'extended', ''}
end
if ARGV[4] == '1' then
  redis.call('SET', KEYS[1], ARGV[1], 'PX', ARGV[2])
  redis.call('DEL', KEYS[2])
  return {'preempted', cur}
end
return {'conflict', cur}
`;

const RELEASE = `
local cur = redis.call('GET', KEYS[1])
if not cur then return {'none', ''} end
if cjson.decode(cur).keyId ~= ARGV[1] then return {'notyours', cur} end
redis.call('DEL', KEYS[1])
redis.call('DEL', KEYS[2])
return {'released', cur}
`;

// The job payload is stamped with expiresAt by appending, never by decoding it.
// Lua's cjson has one representation for an empty table, so a round-trip turns
// `"components":[]` — "this script needs nothing", the declaration that buys back
// 96KB — into `"components":{}`, which the device cannot deserialize. It rejects
// the whole frame, so the job silently never arrives. The payload also carries
// user script text and base64, none of which is worth re-encoding to insert one
// number. ARGV[2] is a JSON object, so it ends in '}' and is never empty.
const SUBMIT_JOB = `
local cur = redis.call('GET', KEYS[1])
if not cur then return {'nolock', '0'} end
if cjson.decode(cur).keyId ~= ARGV[1] then return {'notyours', '0'} end
local ttl = redis.call('PTTL', KEYS[1])
if ttl <= 0 then return {'nolock', '0'} end
local job = string.sub(ARGV[2], 1, -2) .. ',"expiresAt":' .. (tonumber(ARGV[3]) + ttl) .. '}'
redis.call('SET', KEYS[2], job, 'PX', ttl)
return {'ok', tostring(ttl)}
`;

export type AcquireResult =
	| { status: 'acquired' | 'extended'; lock: Lock }
	| { status: 'conflict' | 'preempted'; prev: Lock };

/** @upstash/redis auto-parses JSON-looking strings in eval results. */
function asLock(value: unknown): Lock | null {
	if (!value) return null;
	return typeof value === 'string' ? (JSON.parse(value) as Lock) : (value as Lock);
}

export async function acquireLock(
	r: Redis,
	opts: { keyId: string; name: string; durationMs: number; override: boolean }
): Promise<AcquireResult> {
	const lock: Lock = {
		keyId: opts.keyId,
		name: opts.name,
		expiresAt: Date.now() + opts.durationMs
	};
	const [status, prev] = (await r.eval(
		ACQUIRE,
		[REDIS.lock, REDIS.jobCurrent],
		[JSON.stringify(lock), String(opts.durationMs), opts.keyId, opts.override ? '1' : '0']
	)) as [string, unknown];

	if (status === 'acquired' || status === 'extended') return { status, lock };
	return { status: status as 'conflict' | 'preempted', prev: asLock(prev)! };
}

export async function releaseLock(
	r: Redis,
	keyId: string
): Promise<{ status: 'released' | 'none' | 'notyours'; prev: Lock | null }> {
	const [status, prev] = (await r.eval(RELEASE, [REDIS.lock, REDIS.jobCurrent], [keyId])) as [
		string,
		unknown
	];
	return { status: status as 'released' | 'none' | 'notyours', prev: asLock(prev) };
}

/** Store the job under the lock's remaining TTL, never the requested duration. */
export async function submitJob(
	r: Redis,
	keyId: string,
	job: Omit<Job, 'expiresAt'>
): Promise<{ status: 'ok'; ttlMs: number } | { status: 'nolock' | 'notyours' }> {
	const [status, ttl] = (await r.eval(
		SUBMIT_JOB,
		[REDIS.lock, REDIS.jobCurrent],
		[keyId, JSON.stringify(job), String(Date.now())]
	)) as [string, string];

	if (status !== 'ok') return { status: status as 'nolock' | 'notyours' };
	return { status: 'ok', ttlMs: Number(ttl) };
}

export async function getLock(r: Redis): Promise<Lock | null> {
	return await r.get<Lock>(REDIS.lock);
}

/** Admin kill switch: drop lock + job unconditionally. */
export async function forceRelease(r: Redis): Promise<void> {
	await r.del(REDIS.lock, REDIS.jobCurrent);
}
