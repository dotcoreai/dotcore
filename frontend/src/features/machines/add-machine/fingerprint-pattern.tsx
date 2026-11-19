import {
  type FingerprintPattern,
  FingerprintPatternColor,
} from "@/gen/dotcore/site/v1/membership_pb";
import { cn } from "@/lib/utils";

const PATTERN_SIZE = 5;
const GROUP_LENGTH = 4;

/**
 * The key fingerprint pattern the joining machine shows too, on the fixed dark backdrop of its
 * terminal, so the operator can match the two screens by eye.
 */
export function FingerprintPatternView({ pattern }: { pattern: FingerprintPattern }) {
  return (
    <div
      aria-label="Fingerprint pattern"
      className="grid flex-none rounded-lg bg-console-terminal p-1.5"
      role="img"
      style={{ gridTemplateColumns: `repeat(${PATTERN_SIZE}, 1.05rem)` }}
    >
      {pattern.cells.map((cell, index) => {
        const color = cell > 0 ? pattern.colors[cell - 1] : undefined;
        return (
          <i
            key={index}
            className={cn("size-[1.05rem]", color ? COLOR_STYLES[color] : undefined)}
            data-color={color ? FingerprintPatternColor[color] : "empty"}
          />
        );
      })}
    </div>
  );
}

/** A key fingerprint in groups of four characters, eight to a line, to compare piece by piece. */
export function GroupedFingerprint({ fingerprint }: { fingerprint: string }) {
  const groups = fingerprint.match(new RegExp(`.{1,${GROUP_LENGTH}}`, "g")) ?? [];

  return (
    <code className="grid grid-cols-4 justify-start gap-x-3 gap-y-0.5 font-mono text-[12.5px] sm:grid-cols-[repeat(8,auto)] sm:gap-x-2.5">
      {groups.map((group, index) => (
        <span key={index}>{group}</span>
      ))}
    </code>
  );
}

const COLOR_STYLES: Partial<Record<FingerprintPatternColor, string>> = {
  [FingerprintPatternColor.RED]: "bg-console-pattern-red",
  [FingerprintPatternColor.GREEN]: "bg-console-pattern-green",
  [FingerprintPatternColor.YELLOW]: "bg-console-pattern-yellow",
  [FingerprintPatternColor.BLUE]: "bg-console-pattern-blue",
  [FingerprintPatternColor.MAGENTA]: "bg-console-pattern-magenta",
  [FingerprintPatternColor.CYAN]: "bg-console-pattern-cyan",
};
