import { HISTORY_LENGTH, REDIS, type HistoryEntry } from '@traffic-light/protocol';
import type { Redis } from '@upstash/redis';

export async function pushHistory(r: Redis, entry: HistoryEntry): Promise<void> {
	await r.lpush(REDIS.history, JSON.stringify(entry));
	await r.ltrim(REDIS.history, 0, HISTORY_LENGTH - 1);
}

export async function getHistory(r: Redis): Promise<HistoryEntry[]> {
	return await r.lrange<HistoryEntry>(REDIS.history, 0, HISTORY_LENGTH - 1);
}
