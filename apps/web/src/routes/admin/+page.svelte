<script lang="ts">
	import { enhance } from '$app/forms';
	import type { SubmitFunction } from '@sveltejs/kit';
	import type { PageProps } from './$types';

	let { data, form }: PageProps = $props();

	// The editor shows the server's idle script until the admin edits it;
	// the draft then wins, so unrelated invalidations don't clobber edits.
	let draft = $state<string | null>(null);
	const script = $derived(draft ?? data.idle ?? '');

	// Which form was submitted last — the `form` result is global to the page,
	// so this routes success and error feedback to the right form.
	let lastAction = $state<string | null>(null);

	// Actions whose errors render inline next to their form; the global banner
	// is suppressed for these and kept for the rest (login, createKey).
	const INLINE_ERRORS = [
		'setIdle',
		'testLights',
		'endTest',
		'setHistoryVisibility',
		'clearHistory',
		'kill'
	];
	const errorShownInline = $derived(
		lastAction !== null &&
			(INLINE_ERRORS.includes(lastAction) || lastAction.startsWith('revokeKey:'))
	);

	function submit(
		action: string,
		opts: { confirm?: string; keep?: boolean } = {}
	): SubmitFunction {
		return ({ cancel }) => {
			if (opts.confirm && !confirm(opts.confirm)) {
				cancel();
				return;
			}
			lastAction = action;
			// keep: default enhance behavior minus the form reset, so a failed
			// submit (e.g. a Rhai compile error) doesn't wipe the fields.
			if (opts.keep) return ({ update }) => update({ reset: false });
		};
	}

	function minutes(ms: number): number {
		return Math.round(ms / 60_000);
	}

	function expiry(ts: number): string {
		return new Date(ts).toLocaleTimeString();
	}

	function relative(ts: number): string {
		const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
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
	<title>traffic light · admin</title>
</svelte:head>

<div class="page">
	{#snippet done(action: string, message: string)}
		{#if form?.ok && lastAction === action}
			{#key form}
				<span class="done" role="status">✓ {message}</span>
			{/key}
		{/if}
	{/snippet}

	{#snippet err(action: string, mono: boolean = false)}
		{#if form?.error && lastAction === action}
			<span class="fail" class:mono role="alert">{form.error}</span>
		{/if}
	{/snippet}

	{#if !data.authed}
		<div class="login-wrap">
			<form class="panel login" method="POST" action="?/login" use:enhance={submit('login')}>
				<h1>admin</h1>
				{#if form?.error}
					<p class="error">{form.error}</p>
				{/if}
				<label>
					password
					<input type="password" name="password" required autocomplete="current-password" />
				</label>
				<button class="primary" type="submit">log in</button>
			</form>
		</div>
	{:else}
		<header>
			<h1>traffic light admin</h1>
			<form method="POST" action="?/logout" use:enhance={submit('logout')}>
				<button class="ghost" type="submit">log out</button>
			</form>
		</header>

		{#if form?.error && !errorShownInline}
			<div class="banner error">{form.error}</div>
		{/if}

		{#if form?.token}
			<div class="banner token">
				<strong>API key created.</strong>
				<span>Copy it now — it won't be shown again.</span>
				<code>{form.token}</code>
			</div>
		{/if}

		<main>
			<section class="panel">
				<h2>lock</h2>
				{#if data.lock}
					<p>
						held by <strong>{data.lock.name}</strong>
						<span class="muted">({data.lock.keyId})</span>
						· expires {expiry(data.lock.expiresAt)}
					</p>
				{:else}
					<p class="muted">no lock held — the light is free</p>
				{/if}
				<form
					method="POST"
					action="?/kill"
					use:enhance={submit('kill', {
						confirm: 'Force release the lock and kill any running script. Continue?'
					})}
				>
					<button class="danger" type="submit">force release / kill script</button>
					{@render done('kill', 'released')}
					{@render err('kill')}
				</form>
			</section>

			<section class="panel">
				<h2>lamp test</h2>
				<p class="caption">holds the lamps for 60s, then the idle script resumes</p>
				{#if !data.online}
					<p class="warn">
						device is offline — the test job will wait for it to reconnect and may expire first
					</p>
				{/if}
				<form method="POST" action="?/testLights" use:enhance={submit('testLights', { keep: true })}>
					<div class="lamp-toggles">
						<label class="checkbox">
							<input type="checkbox" name="r" />
							<span class="lamp-dot red"></span>
							red
						</label>
						<label class="checkbox">
							<input type="checkbox" name="y" />
							<span class="lamp-dot yellow"></span>
							yellow
						</label>
						<label class="checkbox">
							<input type="checkbox" name="g" />
							<span class="lamp-dot green"></span>
							green
						</label>
					</div>
					<button class="primary" type="submit">send to light</button>
					{@render done('testLights', 'sent to light')}
					{@render err('testLights')}
				</form>
				<form class="end-test" method="POST" action="?/endTest" use:enhance={submit('endTest')}>
					<button class="ghost" type="submit">end test</button>
					{@render done('endTest', 'test ended')}
					{@render err('endTest')}
				</form>
			</section>

			<section class="panel">
				<h2>history</h2>
				<form
					class="history-toggle"
					method="POST"
					action="?/setHistoryVisibility"
					use:enhance={submit('setHistoryVisibility', { keep: true })}
				>
					<label class="checkbox">
						<input
							type="checkbox"
							name="historyPublic"
							checked={data.historyPublic}
							onchange={(e) => e.currentTarget.form?.requestSubmit()}
						/>
						show history on public dashboard
					</label>
					{@render done('setHistoryVisibility', 'saved')}
					{@render err('setHistoryVisibility')}
				</form>
				{#if data.history.length === 0}
					<p class="muted">
						no runs yet
						{@render done('clearHistory', 'cleared')}
					</p>
				{:else}
					<ul class="history">
						{#each data.history as h (h.jobId)}
							<li>
								<div class="row">
									<span class="name">{h.name}</span>
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
					<form
						method="POST"
						action="?/clearHistory"
						use:enhance={submit('clearHistory', { confirm: 'Clear all history entries?' })}
					>
						<button class="danger small" type="submit">clear history</button>
						{@render err('clearHistory')}
					</form>
				{/if}
			</section>

			<section class="panel">
				<h2>API keys</h2>
				{#if data.keys.length === 0}
					<p class="muted">no keys yet</p>
				{:else}
					<div class="table-wrap">
						<table>
							<thead>
								<tr>
									<th>name</th>
									<th>id</th>
									<th>max lock</th>
									<th>flags</th>
									<th></th>
								</tr>
							</thead>
							<tbody>
								{#each data.keys as key (key.id)}
									<tr class:revoked={key.revoked}>
										<td>{key.name}</td>
										<td><code>{key.id}</code></td>
										<td>{minutes(key.maxLockMs)} min</td>
										<td>
											{#if key.override}<span class="tag override">override</span>{/if}
											{#if key.revoked}<span class="tag dead">revoked</span>{/if}
										</td>
										<td>
											{#if !key.revoked}
												<form
													method="POST"
													action="?/revokeKey"
													use:enhance={submit(`revokeKey:${key.id}`, {
														confirm: `Revoke key "${key.name}"?`
													})}
												>
													<input type="hidden" name="id" value={key.id} />
													<button class="danger small" type="submit">revoke</button>
													{@render err(`revokeKey:${key.id}`)}
												</form>
											{:else}
												{@render done(`revokeKey:${key.id}`, 'revoked')}
											{/if}
										</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}

				<form class="create-key" method="POST" action="?/createKey" use:enhance={submit('createKey')}>
					<h3>create key</h3>
					<div class="fields">
						<label>
							name
							<input type="text" name="name" required />
						</label>
						<label>
							max lock (minutes)
							<input type="number" name="maxLockMinutes" min="1" step="1" value="15" required />
						</label>
						<label class="checkbox">
							<input type="checkbox" name="override" />
							override
							<span class="caption">can preempt other keys' locks</span>
						</label>
					</div>
					<button class="primary" type="submit">create</button>
				</form>
			</section>

			<section class="panel">
				<h2>idle script</h2>
				<p class="caption">what the light runs when nobody holds a lock</p>
				{#if !data.online}
					<p class="warn">device is offline — a saved script applies when it reconnects</p>
				{/if}
				<form method="POST" action="?/setIdle" use:enhance={submit('setIdle', { keep: true })}>
					<textarea
						name="script"
						rows="14"
						spellcheck="false"
						bind:value={() => script, (v) => (draft = v)}
					></textarea>
					<button class="primary" type="submit">save idle script</button>
					{@render done('setIdle', 'saved — the light picks it up immediately')}
					{@render err('setIdle', true)}
				</form>
			</section>
		</main>

		<footer>
			<a href="/">← dashboard</a>
		</footer>
	{/if}
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
		gap: 1.5rem;
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

	h1 {
		margin: 0;
		font-size: 1.3rem;
		font-weight: 650;
	}

	h2 {
		margin: 0 0 0.75rem;
		font-size: 0.75rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.12em;
		color: var(--muted);
	}

	h3 {
		margin: 0 0 0.6rem;
		font-size: 0.85rem;
		font-weight: 600;
	}

	header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: 1rem;
	}

	main {
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

	.login-wrap {
		flex: 1;
		display: flex;
		align-items: center;
		justify-content: center;
	}

	.login {
		display: flex;
		flex-direction: column;
		gap: 1rem;
		width: min(100%, 320px);
	}

	.muted {
		color: var(--muted);
	}

	.caption {
		margin: 0 0 0.75rem;
		font-size: 0.78rem;
		color: var(--muted);
	}

	.warn {
		margin: 0 0 0.75rem;
		font-size: 0.78rem;
		color: #ffb347;
	}

	.done {
		margin-left: 0.6rem;
		font-size: 0.8rem;
		color: var(--green);
		animation: fade-done 0.4s ease 2.6s forwards;
	}

	@keyframes fade-done {
		to {
			opacity: 0;
		}
	}

	.fail {
		margin-left: 0.6rem;
		font-size: 0.8rem;
		color: var(--red);
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}

	.fail.mono {
		display: block;
		margin: 0.5rem 0 0;
		font-family: ui-monospace, 'SF Mono', Menlo, monospace;
	}

	.history-toggle {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
	}

	.banner {
		border: 1px solid;
		border-radius: 10px;
		padding: 0.8rem 1rem;
		font-size: 0.9rem;
	}

	.banner.error,
	.error {
		color: var(--red);
		border-color: color-mix(in srgb, var(--red) 40%, transparent);
		background: color-mix(in srgb, var(--red) 8%, transparent);
	}

	p.error {
		margin: 0;
		padding: 0.5rem 0.7rem;
		border: 1px solid color-mix(in srgb, var(--red) 40%, transparent);
		border-radius: 8px;
		font-size: 0.85rem;
	}

	.banner.token {
		display: flex;
		flex-direction: column;
		gap: 0.4rem;
		color: var(--green);
		border-color: color-mix(in srgb, var(--green) 40%, transparent);
		background: color-mix(in srgb, var(--green) 8%, transparent);
	}

	.banner.token span {
		font-size: 0.8rem;
		color: var(--yellow);
	}

	.banner.token code {
		user-select: all;
		font-size: 0.95rem;
		padding: 0.5rem 0.7rem;
		border-radius: 8px;
		background: var(--bg);
		border: 1px solid var(--border);
		color: var(--text);
		overflow-x: auto;
	}

	code {
		font-family: ui-monospace, 'SF Mono', Menlo, monospace;
		font-size: 0.8rem;
		color: var(--muted);
	}

	label {
		display: flex;
		flex-direction: column;
		gap: 0.35rem;
		font-size: 0.8rem;
		color: var(--muted);
	}

	input[type='text'],
	input[type='number'],
	input[type='password'] {
		background: var(--bg);
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 0.55rem 0.7rem;
		color: var(--text);
		font: inherit;
	}

	input:focus-visible,
	textarea:focus-visible {
		outline: 2px solid color-mix(in srgb, var(--yellow) 60%, transparent);
		outline-offset: 1px;
	}

	label.checkbox {
		flex-direction: row;
		flex-wrap: wrap;
		align-items: center;
		gap: 0.5rem;
	}

	label.checkbox .caption {
		margin: 0;
		flex-basis: 100%;
	}

	.fields {
		display: flex;
		flex-wrap: wrap;
		gap: 1rem;
		align-items: flex-start;
		margin-bottom: 0.9rem;
	}

	.table-wrap {
		overflow-x: auto;
		margin-bottom: 1.25rem;
	}

	table {
		width: 100%;
		border-collapse: collapse;
		font-size: 0.85rem;
	}

	th {
		text-align: left;
		font-size: 0.7rem;
		text-transform: uppercase;
		letter-spacing: 0.1em;
		color: var(--muted);
		font-weight: 600;
		padding: 0.4rem 0.8rem 0.4rem 0;
	}

	td {
		padding: 0.5rem 0.8rem 0.5rem 0;
		border-top: 1px solid var(--border);
	}

	tr.revoked td {
		opacity: 0.45;
	}

	.tag {
		display: inline-block;
		font-size: 0.68rem;
		padding: 0.1em 0.55em;
		border-radius: 999px;
		border: 1px solid;
	}

	.tag.override {
		color: var(--yellow);
		border-color: color-mix(in srgb, var(--yellow) 40%, transparent);
	}

	.tag.dead {
		color: var(--muted);
		border-color: var(--border);
	}

	.create-key {
		border-top: 1px solid var(--border);
		padding-top: 1rem;
	}

	.lamp-toggles {
		display: flex;
		flex-wrap: wrap;
		gap: 1.25rem;
		margin-bottom: 0.9rem;
	}

	.lamp-dot {
		width: 0.7em;
		height: 0.7em;
		border-radius: 50%;
	}

	.lamp-dot.red {
		background: var(--red);
		box-shadow: 0 0 6px color-mix(in srgb, var(--red) 60%, transparent);
	}

	.lamp-dot.yellow {
		background: var(--yellow);
		box-shadow: 0 0 6px color-mix(in srgb, var(--yellow) 60%, transparent);
	}

	.lamp-dot.green {
		background: var(--green);
		box-shadow: 0 0 6px color-mix(in srgb, var(--green) 60%, transparent);
	}

	.end-test {
		margin-top: 0.6rem;
	}

	.history {
		list-style: none;
		margin: 0.75rem 0 1rem;
		padding: 0;
		display: flex;
		flex-direction: column;
	}

	.history li {
		padding: 0.6rem 0;
		border-top: 1px solid var(--border);
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

	textarea {
		width: 100%;
		box-sizing: border-box;
		background: var(--bg);
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 0.7rem;
		color: var(--text);
		font-family: ui-monospace, 'SF Mono', Menlo, monospace;
		font-size: 0.82rem;
		line-height: 1.5;
		resize: vertical;
		margin-bottom: 0.75rem;
	}

	button {
		font: inherit;
		font-size: 0.82rem;
		font-weight: 600;
		border-radius: 8px;
		padding: 0.5rem 0.9rem;
		cursor: pointer;
		border: 1px solid transparent;
	}

	button.primary {
		background: var(--green);
		color: #06210f;
	}

	button.primary:hover {
		filter: brightness(1.1);
	}

	button.danger {
		background: color-mix(in srgb, var(--red) 12%, transparent);
		border-color: color-mix(in srgb, var(--red) 45%, transparent);
		color: var(--red);
	}

	button.danger:hover {
		background: color-mix(in srgb, var(--red) 22%, transparent);
	}

	button.small {
		font-size: 0.72rem;
		padding: 0.3rem 0.65rem;
	}

	button.ghost {
		background: transparent;
		border-color: var(--border);
		color: var(--muted);
	}

	button.ghost:hover {
		color: var(--text);
	}

	footer {
		font-size: 0.78rem;
	}

	footer a {
		color: var(--muted);
		text-decoration: none;
	}

	footer a:hover {
		color: var(--text);
	}
</style>
