import { cn } from "@/lib/utils";
import { useResourceColors } from "@/theme/resource-color-context";

import { OtherDetail, OtherSwatch, SegmentDetail } from "./memory-bar";
import { type DetailLine, sum } from "./memory-text";
import { type MemoryRow } from "./site-memory";

/** A row's breakdown for a click or tap: the row detail, then every drawn part of its bar. */
export function RowBreakdown({ detail, row }: { detail: readonly DetailLine[]; row: MemoryRow }) {
  const segments = row.spans
    .flatMap((span) => span.segments)
    .filter((segment) => segment.bytes > 0n);
  const otherBytes = sum(row.spans.map((span) => span.otherBytes));
  const { colorOf } = useResourceColors();

  return (
    <>
      <div className="flex flex-col gap-0.5">
        <DetailLines lines={detail} />
      </div>
      {segments.map((segment) => (
        <div
          className={breakdownEntryClass}
          key={`${segment.machineId}:${segment.owner ?? "model"}:${segment.deploymentId}`}
        >
          <span
            aria-hidden="true"
            className="mt-1 size-2 rounded-xs"
            style={{ backgroundColor: colorOf(segment.colorKey) }}
          />
          <SegmentDetail parts={row.spans} segment={segment} />
        </div>
      ))}
      {otherBytes > 0n ? (
        <div className={breakdownEntryClass}>
          <OtherSwatch />
          <OtherDetail parts={row.spans} />
        </div>
      ) : null}
    </>
  );
}

export function DetailLines({ lines }: { lines: readonly DetailLine[] }) {
  return lines.map((line) => (
    <span className={cn(line.indent && "pl-3", line.strong && "font-semibold")} key={line.key}>
      {line.text}
    </span>
  ));
}

// One drawn part in a row's breakdown: its swatch, then its detail lines beside it.
const breakdownEntryClass =
  "grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 gap-y-0.5 border-t border-border pt-2 [&>:not(:first-child)]:col-start-2";
