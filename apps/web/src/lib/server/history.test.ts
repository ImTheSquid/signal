import { Redis } from '@upstash/redis';
import { beforeEach, describe, expect, it } from 'vitest';
import {
	clearHistory,
	collapseHistory,
	getHistory,
	getSettings,
	healLostEntries,
	pushHistory,
	setSettings
} from './history';

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

describe('history collapsing', () => {
	const done = (keyId: string, jobId: string, start: number, extra: object = {}) => ({
		keyId,
		name: keyId,
		jobId,
		start,
		end: start + 1_000,
		result: 'ok' as const,
		...extra
	});

	it('merges consecutive terminal runs from the same key', async () => {
		const t = Date.now();
		await pushHistory(r, done('spam', 'j1', t - 30_000));
		await pushHistory(r, done('spam', 'j2', t - 20_000));
		await pushHistory(r, done('spam', 'j3', t - 10_000));

		const history = await getHistory(r);
		expect(history).toHaveLength(1);
		expect(history[0].runs).toBe(3);
		expect(history[0].jobId).toBe('j3'); // newest wins
		expect(history[0].start).toBe(t - 30_000); // stretched to oldest
	});

	it('never merges across a different key or into a running entry', async () => {
		const t = Date.now();
		await pushHistory(r, done('a', 'a1', t - 50_000));
		await pushHistory(r, done('b', 'b1', t - 40_000));
		await pushHistory(r, done('a', 'a2', t - 30_000));
		await pushHistory(r, { ...done('a', 'a3', t - 5_000), end: null, result: 'running' });

		const history = await getHistory(r);
		expect(history.map((h) => [h.jobId, h.runs ?? 1])).toEqual([
			['a3', 1], // running — untouched
			['a2', 1],
			['b1', 1],
			['a1', 1]
		]);
	});

	it('accumulates runs across repeated collapses and keeps the newest error', async () => {
		const t = Date.now();
		await pushHistory(r, done('spam', 'j1', t - 40_000, { error: 'old boom' }));
		await pushHistory(r, done('spam', 'j2', t - 30_000));
		await collapseHistory(r);
		await pushHistory(r, done('spam', 'j3', t - 10_000));

		const history = await getHistory(r);
		expect(history).toHaveLength(1);
		expect(history[0].runs).toBe(3);
		expect(history[0].error).toBe('old boom'); // newest has none; older streak error kept
	});
});
