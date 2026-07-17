import {
	HISTORY_LENGTH,
	REDIS,
	SettingsSchema,
	type HistoryEntry,
	type Settings
} from '@traffic-light/protocol';
import type { Redis } from '@upstash/redis';

export async function getSettings(r: Redis): Promise<Settings> {
	const raw = await r.get(REDIS.settings);
	const parsed = SettingsSchema.safeParse(raw ?? {});
	return parsed.success ? parsed.data : SettingsSchema.parse({});
}

export async function setSettings(r: Redis, settings: Settings): Promise<void> {
	await r.set(REDIS.settings, JSON.stringify(settings));
}

export async function clearHistory(r: Redis): Promise<void> {
	await r.del(REDIS.history);
}

export async function pushHistory(r: Redis, entry: HistoryEntry): Promise<void> {
	await r.lpush(REDIS.history, JSON.stringify(entry));
	await r.ltrim(REDIS.history, 0, HISTORY_LENGTH - 1);
}

export async function getHistory(r: Redis): Promise<HistoryEntry[]> {
	return await r.lrange<HistoryEntry>(REDIS.history, 0, HISTORY_LENGTH - 1);
}

const CLOSE_LOST = `
local n = redis.call('LLEN', KEYS[1])
for i = 0, n - 1 do
  local e = cjson.decode(redis.call('LINDEX', KEYS[1], i))
  if e.jobId == ARGV[1] and e.result == 'running' then
    e.result = 'lost'
    e['end'] = tonumber(ARGV[2])
    redis.call('LSET', KEYS[1], i, cjson.encode(e))
    break
  end
end
return 1
`;

/** A history entry only leaves "running" via the device's job_done. If the
 *  device vanished mid-job (or never picked it up), the entry is orphaned —
 *  close it as "lost" once the job is provably not running anywhere. */
export async function healLostEntries(
	r: Redis,
	history: HistoryEntry[],
	currentJobId: string | undefined,
	deviceRunning: string | undefined
): Promise<void> {
	const grace = 15_000; // delivery window between submit and device pickup
	for (const entry of history) {
		if (
			entry.result === 'running' &&
			Date.now() - entry.start > grace &&
			entry.jobId !== currentJobId &&
			entry.jobId !== deviceRunning
		) {
			entry.result = 'lost';
			entry.end = Date.now();
			await r.eval(CLOSE_LOST, [REDIS.history], [entry.jobId, String(entry.end)]);
		}
	}
}
