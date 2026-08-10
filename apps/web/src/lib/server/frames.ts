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
type HelloIdle = NonNullable<Extract<ServerMsg, { t: 'hello' }>['idle']>;

/**
 * The source, unless there is a lowered form that makes it dead weight.
 *
 * The device loads the artifact and never reads the source, so a frame carrying
 * both spends receive buffer — the scarcest thing it has, and what a reboot loop
 * was once traced to — on bytes it discards. Sending neither is not an option:
 * the device refuses that frame rather than guess.
 */
function sourceUnlessLowered<T extends { script: string; artifact?: string }>(
	record: T
): { script?: string } {
	return record.artifact ? {} : { script: record.script };
}

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
		...sourceUnlessLowered(job),
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
 * The stored record minus its source when that is redundant.
 *
 * Every other field is carried by spread rather than respelled: `IdleSchema` is
 * the wire shape, so a field added to the record reaches the device without a
 * second edit here — which is also why nothing server-side may be stored on it.
 */
export function idleFrameFields(idle: Idle): HelloIdle {
	const { script: _script, ...rest } = idle;
	return { ...rest, ...sourceUnlessLowered(idle) };
}

export function buildIdleFrame(idle: Idle): ServerMsg {
	return { t: 'idle', ...idleFrameFields(idle) };
}

export function buildHelloFrame(job: Job | null, idle: Idle | null, now: number): ServerMsg {
	return {
		t: 'hello',
		job: job === null ? null : jobFrameFields(job, now),
		idle: idle === null ? null : idleFrameFields(idle)
	};
}
