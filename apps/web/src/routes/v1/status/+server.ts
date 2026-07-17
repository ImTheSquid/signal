import { json } from '@sveltejs/kit';
import {
	REDIS,
	SettingsSchema,
	type DeviceState,
	type Job,
	type Lock
} from '@traffic-light/protocol';
import { getHistory, healLostEntries } from '$lib/server/history';
import { redis } from '$lib/server/redis';
import type { RequestHandler } from './$types';

export const GET: RequestHandler = async () => {
	const r = redis();
	const [lock, device, job, rawSettings] = await r.mget<
		[Lock | null, DeviceState | null, Job | null, unknown]
	>(REDIS.lock, REDIS.device, REDIS.jobCurrent, REDIS.settings);
	const settings = SettingsSchema.safeParse(rawSettings ?? {});
	const historyPublic = settings.success ? settings.data.historyPublic : true;

	const history = await getHistory(r);
	await healLostEntries(r, history, job?.jobId, device?.running);

	return json(
		{
			lock,
			device,
			online: device !== null,
			historyPublic,
			history: historyPublic ? history : []
		},
		{
			headers: {
				// The CDN collapses all dashboard pollers into ≤1 origin hit/10s —
				// load-bearing for the Upstash free tier, not an optimization.
				'cache-control': 'public, s-maxage=10, stale-while-revalidate=20'
			}
		}
	);
};
