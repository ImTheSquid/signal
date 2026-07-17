import { Redis } from '@upstash/redis';
import { beforeEach, describe, expect, it } from 'vitest';
import { clearHistory, getHistory, getSettings, healLostEntries, pushHistory, setSettings } from './history';

const r = new Redis({
	url: process.env.UPSTASH_REDIS_REST_URL ?? 'http://localhost:8079',
	token: process.env.UPSTASH_REDIS_REST_TOKEN ?? 'dev_token'
});

beforeEach(async () => {
	await r.flushdb();
});

const entry = (jobId: string, result: 'running' | 'ok', start = Date.now()) => ({
	keyId: 'k',
	name: 'k',
	jobId,
	start,
	end: null,
	result
});

describe('settings', () => {
	it('defaults to public history', async () => {
		expect(await getSettings(r)).toEqual({ historyPublic: true });
	});

	it('round-trips', async () => {
		await setSettings(r, { historyPublic: false });
		expect(await getSettings(r)).toEqual({ historyPublic: false });
	});
});

describe('history healing', () => {
	it('closes an orphaned running entry as lost', async () => {
		await pushHistory(r, entry('gone', 'running', Date.now() - 60_000));
		const history = await getHistory(r);
		await healLostEntries(r, history, undefined, 'idle');

		const healed = await getHistory(r);
		expect(healed[0].result).toBe('lost');
		expect(healed[0].end).not.toBeNull();
	});

	it('leaves the current job and fresh entries alone', async () => {
		await pushHistory(r, entry('current', 'running', Date.now() - 60_000));
		await pushHistory(r, entry('fresh', 'running')); // within delivery grace
		const history = await getHistory(r);
		await healLostEntries(r, history, 'current', 'current');

		const after = await getHistory(r);
		expect(after.map((h) => h.result)).toEqual(['running', 'running']);
	});

	it('clearHistory empties the list', async () => {
		await pushHistory(r, entry('a', 'ok'));
		await clearHistory(r);
		expect(await getHistory(r)).toEqual([]);
	});
});
