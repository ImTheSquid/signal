import { Redis } from '@upstash/redis';
import { env } from '$env/dynamic/private';

let client: Redis | undefined;

export function redis(): Redis {
	if (!client) {
		if (!env.UPSTASH_REDIS_REST_URL || !env.UPSTASH_REDIS_REST_TOKEN) {
			throw new Error('UPSTASH_REDIS_REST_URL and UPSTASH_REDIS_REST_TOKEN must be set');
		}
		client = new Redis({
			url: env.UPSTASH_REDIS_REST_URL,
			token: env.UPSTASH_REDIS_REST_TOKEN
		});
	}
	return client;
}
