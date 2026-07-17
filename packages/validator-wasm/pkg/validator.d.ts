/* tslint:disable */
/* eslint-disable */

/**
 * Compile-check a script against the shared engine configuration.
 * Returns JSON: {"ok":true} or {"ok":false,"error":...,"line":...,"col":...}
 */
export function validate(script: string): string;
