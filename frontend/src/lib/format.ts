/**
 * Shared count, byte-size, quantity, and relative-time formatting for console values, and the
 * reading of typed sizes.
 */

import { safeNumber } from "./safe-integer";

/** A protobuf `google.protobuf.Timestamp`-shaped value: seconds and nanos since the Unix epoch. */
interface ProtoTimestamp {
  nanos: number;
  seconds: bigint;
}

/** GiB with one decimal below 10 GiB and none above, for compact readouts. */
export function formatGib(bytes: bigint): string {
  const gib = Number(bytes) / 1024 ** 3;
  return gib >= 10 ? Math.round(gib).toString() : (Math.round(gib * 10) / 10).toString();
}

/**
 * Formats a byte count in binary units (KiB, MiB, GiB, TiB), choosing the largest unit that keeps
 * the displayed magnitude at least 1.
 */
export function formatBytes(value: bigint): string {
  const parts = formatBytesParts(value);
  return `${parts.value} ${parts.unit}`;
}

/** `formatBytes` as its number and its unit, for readouts that set the unit apart. */
export function formatBytesParts(value: bigint): { unit: string; value: string } {
  let unit = 0;
  let displayValue = Number(value);

  while (displayValue >= 1024 && unit < byteUnits.length - 1) {
    displayValue /= 1024;
    unit += 1;
  }

  const formatted = new Intl.NumberFormat(undefined, { maximumFractionDigits: 1 }).format(
    displayValue,
  );

  // Only the base unit is a spelled-out English noun, so it is the only one that inflects.
  return { unit: unit === 0 && displayValue === 1 ? "byte" : byteUnits[unit], value: formatted };
}

/**
 * Formats a byte range with `formatBytes`, such as "16.1–32.8 GiB". Ends sharing a unit name it
 * once, ends in different units each keep their own, and ends that read the same collapse to one.
 */
export function formatByteRange(min: bigint, max: bigint): string {
  const low = formatBytes(min);
  const high = formatBytes(max);
  if (low === high) return low;

  const [lowValue, lowUnit] = splitUnit(low);
  const [highValue, highUnit] = splitUnit(high);
  return lowUnit === highUnit ? `${lowValue}–${highValue} ${highUnit}` : `${low}–${high}`;
}

/**
 * Formats an unsigned quantity declared in a free-form unit. Byte units (bytes, KiB, MiB, GiB, TiB)
 * climb to the largest fitting unit like `formatBytes`, so 65536 MiB reads "64 GiB". A `tokens`
 * quantity is compacted by `formatCompactCount` and drops the unit name, since callers show it
 * under a label such as "Context size". Any other unit keeps its name after a
 * digit-grouped number. An empty unit formats the bare number.
 */
export function formatQuantity(value: bigint | number, unit: string): string {
  const bytesPerUnit = byteUnitSizes.get(unit);
  if (bytesPerUnit !== undefined) return formatBytes(BigInt(value) * bytesPerUnit);

  if (unit === "tokens") return formatCompactCount(Number(value));

  const formatted = groupedNumber.format(value);
  return unit ? `${formatted} ${unit}` : formatted;
}

/** A unit the size control reads and writes: tokens, or a binary byte unit. */
export type SizeUnit = "tokens" | ByteUnit;

/** A binary byte unit, from bytes to TiB. */
export type ByteUnit = (typeof byteUnits)[number];

/** Whether a free-form unit is one the size control handles. */
export function isSizeUnit(unit: string): unit is SizeUnit {
  return unit === "tokens" || byteUnitSizes.has(unit);
}

/** How many bytes one of a byte unit holds. */
export function bytesPerUnit(unit: ByteUnit): number {
  return Number(byteUnitSizes.get(unit)!);
}

/**
 * Reads a typed size as a whole number of the given unit, or undefined when the text isn't one. A
 * bare number is already in the unit. Byte sizes take a binary prefix (k, m, g, t, with or without
 * "b" or "ib") or a lone "b" for bytes; token counts take a binary k or m. A size that isn't whole
 * in the unit is refused, never rounded, so "512m" isn't a GiB count.
 */
export function parseSize(text: string, unit: SizeUnit): number | undefined {
  const tokens = unit === "tokens";
  const match = (tokens ? tokenSizePattern : byteSizePattern).exec(text.trim());
  if (!match) return undefined;

  const [, whole, fraction = "", prefix, bytes] = match;
  const perUnit = tokens ? 1n : byteUnitSizes.get(unit)!;
  const multiplier = prefix ? sizePrefixes[prefix.toLowerCase()] : bytes ? 1n : perUnit;
  const numerator = BigInt(whole + fraction) * multiplier;
  const denominator = 10n ** BigInt(fraction.length) * perUnit;
  return numerator % denominator === 0n ? safeNumber(numerator / denominator) : undefined;
}

/**
 * Formats a size given in a byte unit in the largest unit that shows it exactly with at most two
 * decimals, such as "12 GiB" or "12.25 GiB", else as the number in its own unit, such as
 * "12595 MiB". Digits are never grouped, so `parseSize` reads the text back.
 */
export function formatExactSize(value: number, unit: ByteUnit): string {
  const bytes = BigInt(value) * byteUnitSizes.get(unit)!;
  for (const candidate of [...byteUnits].reverse()) {
    if (candidate === unit) break;

    const size = byteUnitSizes.get(candidate)!;
    if (bytes >= size && (bytes * 100n) % size === 0n)
      return `${Number((bytes * 100n) / size) / 100} ${candidate}`;
  }
  return `${value} ${unit}`;
}

/**
 * Formats a token count exactly: in binary millions or thousands when it is a whole number of
 * them, such as "1M" or "128k", else as digits, such as "233895". A "k" always means 1024, as it
 * does when typed.
 */
export function formatExactTokens(count: number): string {
  if (count >= 1024 ** 2 && count % 1024 ** 2 === 0) return `${count / 1024 ** 2}M`;
  if (count >= 1024 && count % 1024 === 0) return `${count / 1024}k`;
  return String(count);
}

/**
 * Formats a count compactly, such as "250k" or "1.5M". A count that is a multiple of 1024 counts
 * binary thousands and millions (131072 is "128k", 3486720 is "3.3M"); any other counts decimal
 * ones (250000 is "250k"). Counts under 1000 are shown as is.
 */
export function formatCompactCount(count: number): string {
  if (count < 1000) return String(count);

  if (count % 1024 === 0) {
    const thousands = count / 1024;
    return thousands >= 1024 ? `${compactMillions.format(thousands / 1024)}M` : `${thousands}k`;
  }

  const thousands = Math.round(count / 1000);
  return thousands >= 1000 ? `${compactMillions.format(count / 1_000_000)}M` : `${thousands}k`;
}

/**
 * Formats a count followed by its noun, using the singular noun only for a count of one, such as
 * "1 service" or "2 services". Pass `plural` for nouns that do not take a plain "s".
 */
export function formatCount(count: number, singular: string, plural = `${singular}s`): string {
  return `${count} ${count === 1 ? singular : plural}`;
}

export interface RelativeTime {
  /** Relative text for the visible value, e.g. "2 minutes ago". */
  text: string;
  /** Absolute UTC timestamp for a `title` attribute, e.g. "2026-08-06 18:35:00 UTC". */
  title: string;
}

/**
 * Formats a protobuf timestamp as relative text against a reference time, alongside an absolute
 * UTC title for exact-time lookup on hover. Computes once per call; does not tick.
 */
export function formatRelativeTime(value: ProtoTimestamp, now: Date = new Date()): RelativeTime {
  const date = new Date(Number(value.seconds) * 1_000 + Math.trunc(value.nanos / 1_000_000));
  const diffSeconds = (date.getTime() - now.getTime()) / 1000;
  const { seconds, unit } = relativeUnitFor(Math.abs(diffSeconds));

  return {
    text: relativeTimeFormatter.format(Math.round(diffSeconds / seconds), unit),
    title: formatAbsoluteUtc(date),
  };
}

const byteUnits = ["bytes", "KiB", "MiB", "GiB", "TiB"] as const;

const byteUnitSizes = new Map<string, bigint>(
  byteUnits.map((unit, index) => [unit, 1024n ** BigInt(index)]),
);

// A typed byte size: a number with an optional binary prefix and "b" or "ib", or a lone "b".
const byteSizePattern = /^(\d+)(?:\.(\d+))?\s*(?:([kmgt])(?:i?b)?|(b))?$/i;

// A typed token count: a number with an optional binary k or m.
const tokenSizePattern = /^(\d+)(?:\.(\d+))?\s*([km])?$/i;

const sizePrefixes: Record<string, bigint> = {
  g: 1024n ** 3n,
  k: 1024n,
  m: 1024n ** 2n,
  t: 1024n ** 4n,
};

const groupedNumber = new Intl.NumberFormat("en-US");

const compactMillions = new Intl.NumberFormat("en-US", { maximumFractionDigits: 1 });

const relativeTimeFormatter = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });

// Largest unit first: the first threshold the elapsed magnitude clears wins, so 90 seconds reads
// as "a minute ago" rather than "90 seconds ago".
const relativeTimeUnits: ReadonlyArray<{ seconds: number; unit: Intl.RelativeTimeFormatUnit }> = [
  { seconds: 60 * 60 * 24 * 365, unit: "year" },
  { seconds: 60 * 60 * 24 * 30, unit: "month" },
  { seconds: 60 * 60 * 24 * 7, unit: "week" },
  { seconds: 60 * 60 * 24, unit: "day" },
  { seconds: 60 * 60, unit: "hour" },
  { seconds: 60, unit: "minute" },
  { seconds: 1, unit: "second" },
];

function relativeUnitFor(absSeconds: number): {
  seconds: number;
  unit: Intl.RelativeTimeFormatUnit;
} {
  return (
    relativeTimeUnits.find(({ seconds }) => absSeconds >= seconds) ??
    relativeTimeUnits[relativeTimeUnits.length - 1]
  );
}

function formatAbsoluteUtc(date: Date): string {
  const iso = date.toISOString();
  return `${iso.slice(0, 10)} ${iso.slice(11, 19)} UTC`;
}

function splitUnit(formatted: string): [string, string] {
  const space = formatted.lastIndexOf(" ");
  return [formatted.slice(0, space), formatted.slice(space + 1)];
}
