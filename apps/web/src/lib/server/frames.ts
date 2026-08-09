/**
 * The frames the device receives.
 *
 * Built here rather than inline at each send, because the same content reaches
 * the device by two routes — `hello` on every connect, and a push when it
 * changes — and a field added to one route and forgotten on the other is
 * invisible until a device that reconnected behaves differently from one that
 * did not. One builder per frame means there is one place to forget.
 */
import type { Idle, Job, ServerMsg } from '@traffic-light/protocol';

type HelloJob = Extract<ServerMsg, { t: 'hello' }>['job'];

/**
 * The device's view of a job, field by field so server-only fields (`map`,
 * `positions`, `keyId`) cannot ride along.
 *
 * `null` once the job has no time left, which is the caller's signal that there
 * is nothing to run rather than something to run for zero milliseconds.
 */
export function jobFrameFields(job: Job, now: number): HelloJob {
	const ttl_ms = job.expiresAt - now;
	if (ttl_ms <= 0) return null;
	return {
		id: job.jobId,
		holder: job.holder ?? '',
		script: job.script,
		ttl_ms,
		// Without these a device that reconnects mid-job is handed the script with
		// no declaration, so it rebuilds the whole standard library and can no
		// longer fit what it was already running.
		components: job.components,
		artifact: job.artifact
	};
}

export function buildJobFrame(job: Job, now: number): ServerMsg | null {
	const fields = jobFrameFields(job, now);
	return fields === null ? null : { t: 'job', ...fields };
}

/**
 * Spread rather than respelled: `IdleSchema` *is* the wire shape, so a field
 * added to the stored record reaches the device without a second edit here.
 * That is also why nothing server-side may be stored on it.
 */
export function buildIdleFrame(idle: Idle): ServerMsg {
	return { t: 'idle', ...idle };
}

export function buildHelloFrame(job: Job | null, idle: Idle | null, now: number): ServerMsg {
	return {
		t: 'hello',
		job: job === null ? null : jobFrameFields(job, now),
		idle
	};
}
