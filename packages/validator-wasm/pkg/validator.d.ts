/* tslint:disable */
/* eslint-disable */

/**
 * Compile-check a script and lower it to the artifact the device runs.
 *
 * Returns JSON: {"ok":true,"script":...,"artifact":...,"positions":...,"residual":N,
 * "bytes":N,"artifactBytes":M} or {"ok":false,"error":...,"line":...,"col":...}
 *
 * Compiles with the engine the firmware builds, so what validates is what runs.
 * Error positions are rhai's own, against the text as submitted.
 */
export function prepare(script: string): string;
