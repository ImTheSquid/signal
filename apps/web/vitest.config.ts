import { defineConfig } from 'vitest/config';

// Standalone config (not the SvelteKit vite config): server-lib tests construct
// their own Redis client and never touch $env or Svelte components.
export default defineConfig({
	test: {
		include: ['src/lib/server/**/*.test.ts'],
		testTimeout: 15_000
	}
});
