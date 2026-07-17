import { json } from '@sveltejs/kit';
import { REDIS, type DeviceState, type Lock } from '@traffic-light/protocol';
import { getHistory } from '$lib/server/history';
import { redis } from '$lib/server/redis';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = async () => {
	const r = redis();
	const [lock, device] = await r.mget<[Lock | null, DeviceState | null]>(
		REDIS.lock,
		REDIS.device
	);
	const history = await getHistory(r);

	return json(
		{ lock, device, online: device !== null, history },
		{
			headers: {
				// The CDN collapses all dashboard pollers into ≤1 origin hit/10s —
				// load-bearing for the Upstash free tier, not an optimization.
				'cache-control': 'public, s-maxage=10, stale-while-revalidate=20'
			}
		}
	);
};
