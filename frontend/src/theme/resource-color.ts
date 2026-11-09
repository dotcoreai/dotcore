/**
 * Deterministic, stable color derivation for resource identity markers.
 *
 * A pool of live resources gets distinct colors from one shared palette through
 * `assignResourceColors`; colors repeat only once every palette color is in use. A resource outside
 * a pool derives its color from a hash of its stable ID. The color is decoration only and asserts
 * nothing factual about the resource.
 */

const COLOR_PALETTE = [
  "#b27587",
  "#ca6c70",
  "#a0807c",
  "#c57543",
  "#a78151",
  "#9f8418",
  "#8b8a76",
  "#7e8d5e",
  "#799238",
  "#46975b",
  "#579272",
  "#049886",
  "#63908c",
  "#1894a1",
  "#0593bf",
  "#578fa8",
  "#548bd0",
  "#7186b1",
  "#7682d1",
  "#89869a",
  "#947bcc",
  "#9d7ba3",
  "#ad73b8",
  "#bf6d9d",
] as const;

/** The marker colors assigned to one pool of resources. */
export interface ResourceColorAssignment {
  /** The color of each pooled key. */
  colors: ReadonlyMap<string, string>;
  /** The least used color, for a resource about to join the pool. */
  nextColor: string;
}

/**
 * Derives a marker fill color from a stable resource ID using FNV-1a hashing.
 * Pure and deterministic: the same ID always yields the same color across calls and module
 * instances.
 */
export function getResourceColor(key: string): string {
  return COLOR_PALETTE[firstChoice(key)];
}

/**
 * Assigns marker colors to a pool of resource keys, processed in the given order. Each key takes
 * its hash color unless another key already uses it, then the least used color found searching
 * onward from it. Colors therefore repeat only once the whole palette is in use, and a key only
 * moves when an earlier key took its hash color.
 */
export function assignResourceColors(keys: readonly string[]): ResourceColorAssignment {
  const useCounts = new Array<number>(COLOR_PALETTE.length).fill(0);
  const colors = new Map<string, string>();

  for (const key of keys) {
    if (colors.has(key)) continue;

    const slot = leastUsedSlot(useCounts, firstChoice(key));
    useCounts[slot] += 1;
    colors.set(key, COLOR_PALETTE[slot]);
  }

  return { colors, nextColor: COLOR_PALETTE[leastUsedSlot(useCounts, 0)] };
}

function leastUsedSlot(useCounts: readonly number[], start: number): number {
  let best = start;
  for (let offset = 1; offset < useCounts.length; offset++) {
    const slot = (start + offset) % useCounts.length;
    if (useCounts[slot] < useCounts[best]) best = slot;
  }

  return best;
}

function firstChoice(key: string): number {
  return fnv1aHash(key) % COLOR_PALETTE.length;
}

function fnv1aHash(value: string): number {
  let hash = 2166136261; // FNV offset basis (32-bit)

  for (let i = 0; i < value.length; i++) {
    hash ^= value.charCodeAt(i);
    // Math.imul keeps the multiply within 32-bit integer semantics; `hash * 16777619` would need
    // up to 57 bits, exceeding float64's 53-bit mantissa, silently rounding away low bits before
    // `>>> 0` truncates and breaking the avalanche behavior the palette distribution depends on.
    hash = Math.imul(hash, 16777619) >>> 0; // FNV prime (32-bit unsigned)
  }

  return hash >>> 0; // Ensure unsigned 32-bit result
}
