import { createHmac, randomBytes, scryptSync, timingSafeEqual } from 'node:crypto';
import { env } from '$env/dynamic/private';

const SESSION_TTL_MS = 7 * 24 * 60 * 60 * 1000;
export const SESSION_COOKIE = 'tl_admin';

/** Produce the value for ADMIN_PASSWORD_HASH ("salt:hash", scrypt). */
export function hashPassword(password: string): string {
	const salt = randomBytes(16).toString('hex');
	const hash = scryptSync(password, salt, 32).toString('hex');
	return `${salt}:${hash}`;
}

export function verifyPassword(password: string): boolean {
	const stored = env.ADMIN_PASSWORD_HASH;
	const [salt, hash] = stored?.split(':') ?? [];
	if (!salt || !hash) return false;
	const expected = Buffer.from(hash, 'hex');
	const actual = scryptSync(password, salt, 32);
	return expected.length === actual.length && timingSafeEqual(expected, actual);
}

function sign(payload: string): string {
	if (!env.SESSION_SECRET) throw new Error('SESSION_SECRET must be set');
	return createHmac('sha256', env.SESSION_SECRET).update(payload).digest('hex');
}

export function createSessionToken(): string {
	const exp = Date.now() + SESSION_TTL_MS;
	return `${exp}.${sign(String(exp))}`;
}

export function verifySessionToken(token: string | undefined): boolean {
	const [exp, mac] = token?.split('.') ?? [];
	if (!exp || !mac) return false;
	if (Number(exp) < Date.now()) return false;
	const expected = Buffer.from(sign(exp), 'hex');
	const actual = Buffer.from(mac, 'hex');
	return expected.length === actual.length && timingSafeEqual(expected, actual);
}
