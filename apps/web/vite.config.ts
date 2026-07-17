import adapter from '@sveltejs/adapter-vercel';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

export default defineConfig({
	plugins: [
		sveltekit({
			compilerOptions: {
				// Force runes mode for the project, except for libraries. Can be removed in svelte 6.
				runes: ({ filename }) =>
					filename.split(/[/\\]/).includes('node_modules') ? undefined : true
			},

			adapter: adapter()
		})
	],
	ssr: {
		// wasm-pack nodejs-target output is CJS + a .wasm loaded via readFileSync;
		// Vite must not bundle it into the server chunk.
		external: ['@traffic-light/validator-wasm']
	}
});
