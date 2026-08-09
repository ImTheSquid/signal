import { prepare as prepareWasm } from '@traffic-light/validator-wasm';

export interface ValidationError {
	error: string;
	line: number | null;
	col: number | null;
	/** Set when the script was rejected on size rather than failing to parse. */
	tooBig?: boolean;
}

/**
 * The four artifact fields are absent together, from the path that declines to
 * minify and passes the source through — there is no lowered form to report. A
 * caller that reads them must handle `undefined`; the device already treats a
 * missing artifact as "parse the source".
 */
export interface PreparedScript {
	/** Minified, and compile-checked against the engine the ESP32 runs. */
	script: string;
	/** Source Map v3 for `script`. Empty when minification was declined. */
	map: string;
	/** The script lowered to bytecode, base64. What the device runs. */
	artifact?: string;
	/** Program-counter-to-source table for `artifact`. Stays server-side. */
	positions?: string;
	/** Nodes the compiler could not lower, which fall back to rhai's walker and
	 *  keep the tree's per-node cost. Zero for everything in this repo. */
	residual?: number;
	rawBytes: number;
	bytes: number;
	/** Size of the base64 artifact, which is the figure the device's limit is
	 *  actually about — `bytes` measures the source it came from. */
	artifactBytes?: number;
	/** False when the original was passed through because minifying it failed. */
	minified: boolean;
	/** Why minification was declined, when it was. */
	warning?: string;
}

type PrepareResult = ({ ok: true } & PreparedScript) | ({ ok: false } & ValidationError);

/**
 * Compile-check a script with the same Rhai engine config the ESP32 runs, and
 * minify it. Error positions refer to the submitted text, not the minified text.
 *
 * `ok` is dropped: callers put the error straight on the wire, where it would be a
 * field the API never used to return.
 */
export function prepareScript(script: string): PreparedScript | ValidationError {
	const { ok: _ok, ...result } = JSON.parse(prepareWasm(script)) as PrepareResult;
	return result;
}

export function isValidationError(v: PreparedScript | ValidationError): v is ValidationError {
	return 'error' in v;
}
