import { describe, expect, it } from 'vitest';
import { isValidationError, prepareScript } from './validate';

function prepared(script: string) {
	const out = prepareScript(script);
	if (isValidationError(out)) throw new Error(`unexpected rejection: ${out.error}`);
	return out;
}

describe('prepareScript', () => {
	// What the admin idle script now depends on. It also fails if the committed
	// wasm build has drifted from crates/validator, which is a real way for this
	// to break without anyone touching TypeScript.
	it('lowers a script to an artifact the device can load', () => {
		const out = prepared('set_lights(false, false, false);');
		expect(out.artifact).toBeTruthy();
		expect(out.artifactBytes).toBeGreaterThan(0);
		// Anything left as a tree keeps the per-node cost the artifact exists to
		// avoid, and the light is the one paying it.
		expect(out.residual).toBe(0);
	});

	it('minifies without growing the script', () => {
		const out = prepared('// a comment\nset_lights(false, false, false);\n');
		expect(out.minified).toBe(true);
		expect(out.bytes).toBeLessThanOrEqual(out.rawBytes);
	});

	it('reports a parse error against the submitted text', () => {
		const out = prepareScript('set_lights(false, false, false)\nthis is not rhai\n');
		expect(isValidationError(out)).toBe(true);
		if (!isValidationError(out)) return;
		expect(out.line).toBe(2);
	});
});
