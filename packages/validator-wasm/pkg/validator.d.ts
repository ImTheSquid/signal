/* tslint:disable */
/* eslint-disable */

/**
 * Compile-check a script and minify it for the device.
 *
 * Returns JSON: {"ok":true,"script":...,"map":...,"rawBytes":N,"bytes":M,"minified":bool}
 * or {"ok":false,"error":...,"line":...,"col":...}
 *
 * `minify_with_engine` compiles both its input and its own output with the engine it
 * is given, so passing the validation engine makes one call cover the parse check the
 * API needs and a conformance check on the text the device will actually run.
 */
export function prepare(script: string): string;

/**
 * Rewrite the `line N, position M` positions in a device-reported error so they
 * refer to the submitted script rather than the minified text.
 *
 * `minified` is the text the device ran: Source Map v3 counts columns in UTF-16
 * units while rhai counts characters, and converting between them needs that line.
 * Anything that does not resolve is left exactly as it arrived.
 */
export function remap(map_json: string, minified: string, message: string): string;
