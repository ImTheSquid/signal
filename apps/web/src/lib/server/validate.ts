import { prepare as prepareWasm, remap as remapWasm } from '@traffic-light/validator-wasm';

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

type PrepareResult =
	| ({ ok: true } & PreparedScript)
	| ({ ok: false } & ValidationError);

/**
 * Compile-check a script with the same Rhai engine config the ESP32 runs, and
 * minify it. Error positions refer to the submitted text, not the minified text.
 */
export function prepareScript(script: string): PreparedScript | ValidationError {
	const result = JSON.parse(prepareWasm(script)) as PrepareResult;
	if (!result.ok) {
		return {
			error: result.error,
			line: result.line,
			col: result.col,
			tooBig: result.tooBig
		};
	}
	const { script: minified, map, rawBytes, bytes, minified: wasMinified, warning } = result;
	return { script: minified, map, rawBytes, bytes, minified: wasMinified, warning };
}

export function isValidationError(v: PreparedScript | ValidationError): v is ValidationError {
	return 'error' in v;
}

/**
 * Rewrite the positions in a device-reported error so they point at the script as
 * submitted. `minified` must be the text the device actually ran.
 */
export function remapError(map: string, minified: string, message: string): string {
	if (!map) return message;
	return remapWasm(map, minified, message);
}
