/**
 * Shared count, byte-size, quantity, and relative-time formatting for console values.
 */

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

/**
 * Formats a count compactly, such as "250k" or "1.5M". A count that is a multiple of 1024 counts
 * binary thousands (131072 is "128k"); any other counts decimal ones (250000 is "250k"). Counts
 * under 1000 are shown as is.
 */
export function formatCompactCount(count: number): string {
  if (count < 1000) return String(count);

  if (count % 1024 === 0) {
    const thousands = count / 1024;
    return thousands >= 1024 && thousands % 1024 === 0 ? `${thousands / 1024}M` : `${thousands}k`;
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

const byteUnits = ["bytes", "KiB", "MiB", "GiB", "TiB"];

const byteUnitSizes = new Map(byteUnits.map((unit, index) => [unit, 1024n ** BigInt(index)]));

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
