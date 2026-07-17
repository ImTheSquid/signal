// Mint a dev API key against the local docker stack: pnpm exec tsx scripts/seed-dev.ts [name]
import { Redis } from '@upstash/redis';
import { createKey } from '../src/lib/server/keys';

const r = new Redis({
	url: process.env.UPSTASH_REDIS_REST_URL ?? 'http://localhost:8079',
	token: process.env.UPSTASH_REDIS_REST_TOKEN ?? 'dev_token'
});

const name = process.argv[2] ?? 'dev';
const override = process.argv.includes('--override');
const { key, token } = await createKey(r, { name, maxLockMs: 5 * 60_000, override });
console.log(`key id: ${key.id}  name: ${key.name}  override: ${key.override}`);
console.log(`token:  ${token}`);
