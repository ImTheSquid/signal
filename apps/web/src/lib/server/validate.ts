import { prepare as prepareWasm } from '@traffic-light/validator-wasm';

export interface ValidationError {
	error: string;
	line: number | null;
	col: number | null;
	/** Set when the script was rejected on size rather than failing to parse. */
	tooBig?: boolean;
}

export interface PreparedScript {
	/** As submitted, compile-checked against the engine the ESP32 runs. */
	script: string;
	/** The script lowered to bytecode, base64. What the device runs. */
	artifact: string;
	/** Program-counter-to-source table for `artifact`. Stays server-side. */
	positions: string;
	/** Nodes the compiler could not lower, which fall back to rhai's walker and
	 *  keep the tree's per-node cost. Zero for everything in this repo. */
	residual: number;
	/** Size of the submitted source. */
	bytes: number;
	/** Size of the base64 artifact, which is the figure the device's limit is
	 *  actually about — `bytes` measures the source it came from. */
	artifactBytes: number;
}

type PrepareResult = ({ ok: true } & PreparedScript) | ({ ok: false } & ValidationError);

/**
 * Compile-check a script with the same Rhai engine config the ESP32 runs, and
 * lower it to the artifact the device loads. Error positions refer to the
 * submitted text.
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
