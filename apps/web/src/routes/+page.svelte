<script lang="ts">
	interface Lock {
		keyId: string;
		name: string;
		expiresAt: number;
	}

	interface Device {
		lights: { r: boolean; y: boolean; g: boolean };
		running: string;
		heap: number;
		heap_block?: number;
		ops?: number[];
		fw: string;
		ts: number;
	}

	interface HistoryEntry {
		keyId: string;
		name: string;
		jobId: string;
		start: number;
		end: number | null;
		result: 'ok' | 'error' | 'aborted' | 'deadline' | 'preempted' | 'running' | 'lost';
		error?: string;
		// Consecutive terminal runs from the same key collapse into one entry;
		// absent or 1 means a single run.
		runs?: number;
	}

	interface Status {
		lock: Lock | null;
		device: Device | null;
		online: boolean;
		historyPublic: boolean;
		history: HistoryEntry[];
	}

	const POLL_MS = 10_000;

	let status = $state<Status | null>(null);
	let unreachable = $state(false);
	let now = $state(Date.now());

	async function refresh() {
		try {
			const res = await fetch('/v1/status');
			if (!res.ok) throw new Error(`status ${res.status}`);
			const snapshot = await res.json();
			// A live socket outranks a poll that resolved late.
			if (ws?.readyState === WebSocket.OPEN) return;
			status = snapshot;
			unreachable = false;
		} catch {
			if (ws?.readyState === WebSocket.OPEN) return;
			unreachable = true;
		}
	}

	let poll: ReturnType<typeof setInterval> | undefined;

	function startPolling() {
		if (poll) return;
		refresh();
		poll = setInterval(refresh, POLL_MS);
	}

	function stopPolling() {
		clearInterval(poll);
		poll = undefined;
	}

	// Live updates come over a websocket; the 10s polling runs only while the
	// socket is connecting or backing off. Everything stops while the tab is
	// hidden — keeps idle tabs off the Redis free tier.
	let ws: WebSocket | undefined;
	let reconnectTimer: ReturnType<typeof setTimeout> | undefined;
	let backoffMs = 1000;
	// False while hidden or after teardown; gates connects and reconnects.
	let active = false;

	function wsUrl(): string {
		if (import.meta.env.DEV) return 'ws://localhost:3002';
		const proto = location.protocol === 'https:' ? 'wss' : 'ws';
		return `${proto}://${location.host}/api/live`;
	}

	function connect() {
		if (!active || ws) return;
		const socket = new WebSocket(wsUrl());
		ws = socket;
		socket.onopen = () => {
			if (ws !== socket) return;
			backoffMs = 1000;
			stopPolling();
		};
		socket.onmessage = (event) => {
			if (ws !== socket) return;
			try {
				status = JSON.parse(event.data);
				unreachable = false;
			} catch {
				// ignore malformed frames
			}
		};
		socket.onclose = () => {
			if (ws !== socket) return;
			ws = undefined;
			if (!active) return;
			// Poll while reconnecting with backoff.
			startPolling();
			reconnectTimer = setTimeout(connect, backoffMs);
			backoffMs = Math.min(backoffMs * 2, 30_000);
		};
	}

	function disconnect() {
		clearTimeout(reconnectTimer);
		reconnectTimer = undefined;
		const socket = ws;
		ws = undefined; // trips the handlers' identity guards before close fires
		socket?.close();
	}

	function onVisibility() {
		if (document.hidden) {
			active = false;
			disconnect();
			stopPolling();
		} else {
			active = true;
			backoffMs = 1000;
			startPolling();
			connect();
		}
	}

	$effect(() => {
		if (!document.hidden) {
			active = true;
			startPolling();
			connect();
		}
		const tick = setInterval(() => (now = Date.now()), 1000);
		return () => {
			active = false;
			disconnect();
			stopPolling();
			clearInterval(tick);
		};
	});

	const online = $derived(status?.online ?? false);
	const lights = $derived(status?.device?.lights ?? { r: false, y: false, g: false });
	// null once the countdown crosses zero, until the next poll confirms.
	const activeLock = $derived(
		status?.lock && status.lock.expiresAt > now ? status.lock : null
	);

	function countdown(ms: number): string {
		const s = Math.max(0, Math.ceil(ms / 1000));
		return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
	}

	function relative(ts: number): string {
		const s = Math.max(0, Math.floor((now - ts) / 1000));
		if (s < 60) return `${s}s ago`;
		const m = Math.floor(s / 60);
		if (m < 60) return `${m}m ago`;
		const h = Math.floor(m / 60);
		if (h < 24) return `${h}h ago`;
		return `${Math.floor(h / 24)}d ago`;
	}

	function duration(start: number, end: number): string {
		const s = Math.max(0, Math.round((end - start) / 1000));
		if (s < 60) return `${s}s`;
		return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
	}
</script>

<svelte:head>
	<title>traffic light</title>
	<meta name="description" content="an internet-controlled traffic light" />
</svelte:head>

<svelte:document onvisibilitychange={onVisibility} />

<div class="page">
	<header>
		<h1>traffic light</h1>
		<div class="status-line">
			<span class="badge" class:ok={online} class:bad={!online}>
				<span class="dot"></span>
				{online ? 'online' : 'offline'}
			</span>
			{#if online && status?.device}
				<span class="badge">
					{status.device.running === 'idle' ? 'running idle script' : 'running a job'}
				</span>
			{/if}
			{#if unreachable}
				<span class="badge bad">can't reach the API</span>
			{/if}
		</div>
	</header>

	<main>
		<div class="signal-col">
			<svg
				class="signal"
				class:offline={!online}
				viewBox="0 0 140 330"
				role="img"
				aria-label="traffic signal: red {lights.r ? 'on' : 'off'}, yellow {lights.y
					? 'on'
					: 'off'}, green {lights.g ? 'on' : 'off'}"
			>
				<rect class="pole" x="63" y="272" width="14" height="58" rx="4" />
				<rect class="housing" x="30" y="6" width="80" height="268" rx="22" />
				<circle class="lamp r" class:on={online && lights.r} cx="70" cy="64" r="27" />
				<circle class="lamp y" class:on={online && lights.y} cx="70" cy="140" r="27" />
				<circle class="lamp g" class:on={online && lights.g} cx="70" cy="216" r="27" />
			</svg>
			{#if status?.device}
				<p class="device-meta">
					fw {status.device.fw} · heap {Math.round(status.device.heap / 1024)} KB{#if status.device.heap_block}
						({Math.round(status.device.heap_block / 1024)} KB block){/if}
					{#if status.device.ops}
						<br />relay ops {status.device.ops.join(' / ')}
					{/if}
				</p>
			{/if}
		</div>

		<div class="info-col">
			<section class="panel">
				<h2>lock</h2>
				{#if status === null}
					<p class="muted">connecting…</p>
				{:else if activeLock}
					<p class="holder">{activeLock.name}</p>
					<p class="countdown">{countdown(activeLock.expiresAt - now)}</p>
					<p class="muted">until the lock expires</p>
				{:else}
					<p class="free">unlocked</p>
					<p class="muted">the light is free to claim</p>
				{/if}
			</section>

			<section class="panel">
				<h2>history</h2>
				{#if status === null}
					<p class="muted">connecting…</p>
				{:else if !status.historyPublic}
					<p class="muted">history is private</p>
				{:else if status.history.length === 0}
					<p class="muted">no runs yet</p>
				{:else}
					<ul class="history">
						{#each status.history as h (h.jobId)}
							<li>
								<div class="row">
									<span class="name">{h.name}</span>
									{#if h.runs && h.runs > 1}
										<span class="runs">×{h.runs}</span>
									{/if}
									<span class="chip {h.result}">{h.result}</span>
									<span class="when">{relative(h.start)}</span>
									{#if h.end !== null}
										<span class="dur">{duration(h.start, h.end)}</span>
									{/if}
								</div>
								{#if h.error}
									<p class="err">{h.error}</p>
								{/if}
							</li>
						{/each}
					</ul>
				{/if}
			</section>
		</div>
	</main>

	<footer>
		<span>an internet-controlled traffic light</span>
		<a href="/admin">admin</a>
	</footer>
</div>

<style>
	:global(body) {
		margin: 0;
		background: #0b0e14;
	}

	.page {
		--bg: #0b0e14;
		--panel: #121722;
		--border: #202839;
		--text: #e8edf5;
		--muted: #8b96a8;
		--red: #ff4d4d;
		--yellow: #ffd23f;
		--green: #35e06f;

		min-height: 100vh;
		box-sizing: border-box;
		display: flex;
		flex-direction: column;
		gap: 2rem;
		padding: 2rem clamp(1rem, 5vw, 3rem);
		background: var(--bg);
		color: var(--text);
		font-family:
			ui-sans-serif,
			system-ui,
			-apple-system,
			'Segoe UI',
			sans-serif;
	}

	header {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 1rem;
	}

	h1 {
		margin: 0;
		font-size: 1.4rem;
		font-weight: 650;
		letter-spacing: 0.02em;
	}

	.status-line {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
	}

	.badge {
		display: inline-flex;
		align-items: center;
		gap: 0.4em;
		padding: 0.25em 0.7em;
		border: 1px solid var(--border);
		border-radius: 999px;
		font-size: 0.78rem;
		color: var(--muted);
		background: var(--panel);
	}

	.badge .dot {
		width: 0.5em;
		height: 0.5em;
		border-radius: 50%;
		background: currentColor;
	}

	.badge.ok {
		color: var(--green);
		border-color: color-mix(in srgb, var(--green) 35%, transparent);
	}

	.badge.bad {
		color: var(--red);
		border-color: color-mix(in srgb, var(--red) 35%, transparent);
	}

	main {
		display: flex;
		flex-wrap: wrap;
		gap: 2.5rem;
		align-items: flex-start;
		flex: 1;
	}

	.signal-col {
		display: flex;
		flex-direction: column;
		align-items: center;
		gap: 0.75rem;
		margin-inline: auto;
	}

	.signal {
		width: clamp(120px, 22vw, 180px);
		height: auto;
	}

	.pole {
		fill: #1a2030;
	}

	.housing {
		fill: #151b28;
		stroke: #232d42;
		stroke-width: 2;
	}

	.lamp {
		stroke: #0b0e14;
		stroke-width: 3;
		transition:
			fill 0.35s ease,
			filter 0.35s ease;
	}

	.lamp.r {
		fill: #38151a;
	}
	.lamp.y {
		fill: #382e11;
	}
	.lamp.g {
		fill: #123522;
	}

	.lamp.r.on {
		fill: var(--red);
		filter: drop-shadow(0 0 14px rgba(255, 77, 77, 0.75));
	}
	.lamp.y.on {
		fill: var(--yellow);
		filter: drop-shadow(0 0 14px rgba(255, 210, 63, 0.75));
	}
	.lamp.g.on {
		fill: var(--green);
		filter: drop-shadow(0 0 14px rgba(53, 224, 111, 0.75));
	}

	.signal.offline {
		opacity: 0.55;
		filter: saturate(0.15);
	}

	.device-meta {
		margin: 0;
		font-size: 0.72rem;
		color: var(--muted);
		font-variant-numeric: tabular-nums;
	}

	.info-col {
		flex: 1;
		min-width: min(100%, 320px);
		display: flex;
		flex-direction: column;
		gap: 1.25rem;
	}

	.panel {
		background: var(--panel);
		border: 1px solid var(--border);
		border-radius: 12px;
		padding: 1.1rem 1.3rem;
	}

	h2 {
		margin: 0 0 0.75rem;
		font-size: 0.75rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.12em;
		color: var(--muted);
	}

	.muted {
		margin: 0.25rem 0 0;
		color: var(--muted);
		font-size: 0.85rem;
	}

	.holder {
		margin: 0;
		font-size: 1.15rem;
		font-weight: 600;
	}

	.countdown {
		margin: 0.35rem 0 0;
		font-size: 2.2rem;
		font-weight: 650;
		font-variant-numeric: tabular-nums;
		color: var(--yellow);
	}

	.free {
		margin: 0;
		font-size: 1.15rem;
		font-weight: 600;
		color: var(--green);
	}

	.history {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
	}

	.history li {
		padding: 0.6rem 0;
		border-top: 1px solid var(--border);
	}

	.history li:first-child {
		border-top: none;
		padding-top: 0;
	}

	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: 0.6rem;
	}

	.name {
		font-weight: 600;
		font-size: 0.9rem;
	}

	.runs {
		font-size: 0.75rem;
		color: var(--muted);
		font-variant-numeric: tabular-nums;
	}

	.when,
	.dur {
		font-size: 0.78rem;
		color: var(--muted);
		font-variant-numeric: tabular-nums;
	}

	.when {
		margin-left: auto;
	}

	.chip {
		font-size: 0.7rem;
		padding: 0.15em 0.6em;
		border-radius: 999px;
		border: 1px solid;
		text-transform: lowercase;
	}

	.chip.ok {
		color: var(--green);
		border-color: color-mix(in srgb, var(--green) 40%, transparent);
	}
	.chip.error {
		color: var(--red);
		border-color: color-mix(in srgb, var(--red) 40%, transparent);
	}
	.chip.aborted {
		color: #9aa5b4;
		border-color: #3a4557;
	}
	.chip.deadline {
		color: #ffb347;
		border-color: color-mix(in srgb, #ffb347 40%, transparent);
	}
	.chip.preempted {
		color: #b48cff;
		border-color: color-mix(in srgb, #b48cff 40%, transparent);
	}
	.chip.running {
		color: #5cc8ff;
		border-color: color-mix(in srgb, #5cc8ff 40%, transparent);
		animation: pulse 1.6s ease-in-out infinite;
	}
	.chip.lost {
		color: #7a8494;
		border-color: #3a4557;
		border-style: dashed;
	}

	@keyframes pulse {
		50% {
			opacity: 0.5;
		}
	}

	.err {
		margin: 0.3rem 0 0;
		font-size: 0.78rem;
		color: var(--red);
		font-family: ui-monospace, 'SF Mono', Menlo, monospace;
		overflow-wrap: anywhere;
	}

	footer {
		display: flex;
		justify-content: space-between;
		align-items: center;
		font-size: 0.75rem;
		color: var(--muted);
		border-top: 1px solid var(--border);
		padding-top: 1rem;
	}

	footer a {
		color: var(--muted);
		text-decoration: none;
		opacity: 0.7;
	}

	footer a:hover {
		color: var(--text);
		opacity: 1;
	}
</style>
