import { describe, expect, it } from 'vitest';
import type { Idle, Job } from '@traffic-light/protocol';
import { buildHelloFrame, buildIdleFrame, buildJobFrame } from './frames';

const NOW = 1_000_000;

const idle: Idle = {
	script: 'set_lights(false,false,false)',
	rev: 18,
	components: [],
	artifact: 'aWRsZS1hcnRpZmFjdA=='
};

const job: Job = {
	jobId: 'j1',
	keyId: 'k1',
	holder: 'amy',
	script: 'set_lights(true,false,false)',
	map: 'server-only',
	positions: 'server-only',
	rawBytes: 42,
	components: ['math'],
	artifact: 'am9iLWFydGlmYWN0',
	expiresAt: NOW + 30_000
};

describe('idle frames', () => {
	// The device learns the idle script two ways — `hello` on connect, a push on
	// change — and a field added to one route only is invisible until a device
	// that reconnected behaves differently from one that did not.
	it('carry the same declaration on hello as on the push', () => {
		const push = buildIdleFrame(idle);
		const hello = buildHelloFrame(null, idle, NOW);
		if (push.t !== 'idle' || hello.t !== 'hello') throw new Error('wrong frame');
		expect(hello.idle).toEqual({ script: push.script, rev: push.rev, ...idle });
		expect(push.components).toEqual([]);
		expect(push.artifact).toBe(idle.artifact);
	});

	// Absent is not the same as empty: the device reads a missing declaration as
	// the whole standard library, which is what a record written before idle
	// could declare still means.
	it('leave an undeclared record undeclared', () => {
		const push = buildIdleFrame({ script: 'x', rev: 1 });
		if (push.t !== 'idle') throw new Error('wrong frame');
		expect('components' in push).toBe(false);
		expect('artifact' in push).toBe(false);
	});
});

describe('job frames', () => {
	it('carry the same fields on hello as on the push', () => {
		const push = buildJobFrame(job, NOW);
		const hello = buildHelloFrame(job, null, NOW);
		if (push?.t !== 'job' || hello.t !== 'hello') throw new Error('wrong frame');
		const { t: _t, ...pushFields } = push;
		expect(hello.job).toEqual(pushFields);
		expect(pushFields.components).toEqual(['math']);
		expect(pushFields.artifact).toBe(job.artifact);
	});

	// `map` and `positions` exist to translate a device-reported error back to the
	// submitted source, which is work the device has no part in.
	it('leave the server-only fields behind', () => {
		const push = buildJobFrame(job, NOW);
		if (push?.t !== 'job') throw new Error('wrong frame');
		expect('map' in push).toBe(false);
		expect('positions' in push).toBe(false);
		expect('keyId' in push).toBe(false);
		expect('expiresAt' in push).toBe(false);
	});

	// The lock is what bounds a run, so a job whose lock has gone is nothing to
	// run rather than something to run for zero milliseconds.
	it('are not built once the job is out of time', () => {
		expect(buildJobFrame({ ...job, expiresAt: NOW }, NOW)).toBeNull();
		const hello = buildHelloFrame({ ...job, expiresAt: NOW - 1 }, null, NOW);
		if (hello.t !== 'hello') throw new Error('wrong frame');
		expect(hello.job).toBeNull();
	});

	it('count the ttl from the lock expiry', () => {
		const push = buildJobFrame(job, NOW);
		if (push?.t !== 'job') throw new Error('wrong frame');
		expect(push.ttl_ms).toBe(30_000);
	});
});
