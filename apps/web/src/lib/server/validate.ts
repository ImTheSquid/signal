import { validate as validateWasm } from '@traffic-light/validator-wasm';

export interface ValidationError {
	error: string;
	line: number | null;
	col: number | null;
}

/** Compile-check a script with the same Rhai engine config the ESP32 runs. */
export function validateScript(script: string): ValidationError | null {
	const result = JSON.parse(validateWasm(script)) as
		| { ok: true }
		| ({ ok: false } & ValidationError);
	return result.ok ? null : { error: result.error, line: result.line, col: result.col };
}
