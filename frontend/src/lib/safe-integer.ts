/**
 * Whole numbers the console reads from 64-bit wire values and from typed digits. It works with them
 * as JavaScript numbers, which hold integers exactly only up to `Number.MAX_SAFE_INTEGER`.
 */

/** A 64-bit wire value as a number, or undefined when a number can't hold it exactly. */
export function safeNumber(value: bigint): number | undefined {
  return value <= maximumSafeInteger ? Number(value) : undefined;
}

/**
 * The whole number a text of decimal digits holds, or undefined when it holds anything else or a
 * number can't hold it exactly.
 */
export function parseSafeInteger(text: string): number | undefined {
  return /^\d+$/.test(text) ? safeNumber(BigInt(text)) : undefined;
}

const maximumSafeInteger = BigInt(Number.MAX_SAFE_INTEGER);
