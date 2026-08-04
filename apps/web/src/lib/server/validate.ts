import { prepare as prepareWasm } from '@traffic-light/validator-wasm';

export interface ValidationError {
	error: string;
	line: number | null;
	col: number | null;
	/** Set when the script was rejected on size rather than failing to parse. */
	tooBig?: boolean;
}

export interface PreparedScript {
	/** Minified, and compile-checked against the engine the ESP32 runs. */
	script: string;
	/** Source Map v3 for `script`. Empty when minification was declined. */
	map: string;
	rawBytes: number;
	bytes: number;
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
