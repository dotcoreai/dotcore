import { type ReactNode } from "react";

import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatBytes } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useResourceColors } from "@/theme/resource-color-context";

import {
  barLabel,
  type MemoryRowCapacity,
  type MemorySegmentBy,
  sourceSuffix,
  sum,
} from "./memory-text";
import {
  type MemoryKind,
  type MemorySegment,
  type MemorySpan,
  type UnavailableMemory,
} from "./site-memory";

/**
 * One memory bar over `spans`: its parts, by deployment or by machine, over a free-memory track,
 * with the accessible summary of what it draws.
 */
export function MemoryBar({
  capacity,
  deviceLabel,
  highlighted,
  kind,
  segmentBy,
  spans,
  surface,
  totalBytes,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  deviceLabel?: string;
  highlighted?: string;
  kind: MemoryKind;
  segmentBy: MemorySegmentBy;
  spans: readonly MemorySpan[];
  surface: "page" | "panel";
  totalBytes: bigint;
  unavailable: readonly UnavailableMemory[];
}) {
  return (
    <Bar
      ariaLabel={barLabel(kind, spans, unavailable, capacity, deviceLabel)}
      capacity={capacity}
      parts={barParts(segmentBy, spans, highlighted)}
      surface={surface}
      totalBytes={totalBytes}
    />
  );
}

/** The swatch of the hatched fill that stands for use outside any deployment. */
export function OtherSwatch() {
  return <span aria-hidden="true" className={cn("mt-1 size-2 rounded-xs", otherClass)} />;
}

export function SegmentDetail({
  parts,
  segment,
}: {
  parts: readonly MemorySpan[];
  segment: MemorySegment;
}) {
  const machine = parts.find((span) => span.machine.machineId === segment.machineId)?.machine;

  return (
    <>
      <span className="font-semibold">{segment.name}</span>
      {segment.profileId ? <span>{segment.profileId}</span> : null}
      <span>
        {machine ? `${machine.name} · ` : ""}
        {formatBytes(segment.bytes)}
        {sourceSuffix(segment)}
      </span>
    </>
  );
}

export function OtherDetail({ parts }: { parts: readonly MemorySpan[] }) {
  const counted = parts.filter((span) => span.otherBytes > 0n);
  if (counted.length === 1) {
    const [span] = counted;
    return (
      <>
        <span className="font-semibold">{span.otherLabel}</span>
        <span>
          {span.machine.name} · {formatBytes(span.otherBytes)}
        </span>
      </>
    );
  }

  return (
    <>
      <span className="font-semibold">{counted[0]?.otherLabel ?? "Other"}</span>
      {counted.map((span) => (
        <span key={span.machine.machineId}>
          {span.machine.name} · {formatBytes(span.otherBytes)}
        </span>
      ))}
    </>
  );
}

/**
 * One bar: its parts in order over a free-memory track, each giving its detail on hover. It scales
 * to its contents when drawn use exceeds the total, such as a claim past it, so it never overflows.
 * With a `capacity` verdict, a line shows where the total ends or where the fit line sits.
 */
function Bar({
  ariaLabel,
  capacity,
  parts,
  surface,
  totalBytes,
}: {
  ariaLabel: string;
  capacity?: MemoryRowCapacity;
  parts: readonly BarPart[];
  surface: "page" | "panel";
  totalBytes: bigint;
}) {
  const { colorOf } = useResourceColors();
  const drawn = sum(parts.map((part) => part.bytes));
  const scale = Number(drawn > totalBytes ? drawn : totalBytes) || 1;
  const width = (bytes: bigint) => `${(Number(bytes) / scale) * 100}%`;

  return (
    <div className="relative">
      <div
        aria-label={ariaLabel}
        className={cn(
          // Separators are borders inside each segment's width, so they never push segments out.
          "flex h-2.5 overflow-hidden rounded-full bg-foreground/10 [&>*+*]:border-l-2",
          surface === "page" ? "[&>*+*]:border-background" : "[&>*+*]:border-card",
        )}
        role="img"
      >
        {/* A zero-byte part would still draw its separator, so it stays in the text only. */}
        {parts
          .filter((part) => part.bytes > 0n)
          .map((part) => (
            <Tooltip key={part.key}>
              <TooltipTrigger
                className={cn(
                  "h-full shrink-0 transition-[filter,opacity]",
                  part.colorKey === undefined && otherClass,
                  part.dimmed && "opacity-45 grayscale",
                )}
                data-deployment-id={part.deploymentId}
                render={<span />}
                style={{
                  backgroundColor: part.colorKey === undefined ? undefined : colorOf(part.colorKey),
                  width: width(part.bytes),
                }}
              />
              <TooltipContent className="flex-col items-start gap-0.5 text-left">
                {part.detail}
              </TooltipContent>
            </Tooltip>
          ))}
      </div>
      {capacity ? (
        // A tick taller than the bar, so the crossed line reads by shape and not only by the
        // readout color.
        <span
          aria-hidden="true"
          className={cn(
            "pointer-events-none absolute -inset-y-0.5 w-0.5 -translate-x-1/2 rounded-full ring-1",
            capacity.beyondTotal ? "bg-foreground" : "bg-warning-foreground",
            surface === "page" ? "ring-background" : "ring-card",
          )}
          data-line={capacity.beyondTotal ? "total" : "fit"}
          data-slot="memory-capacity-marker"
          style={{ left: width(capacity.lineBytes) }}
        />
      ) : null}
    </div>
  );
}

/** One drawn part of a bar: its size, fill and hover detail. */
interface BarPart {
  bytes: bigint;
  /** The part's marker color; a part without one draws hatched, as use outside any deployment. */
  colorKey?: string;
  deploymentId?: string;
  detail: ReactNode;
  /** Grays the part out while another deployment is highlighted. */
  dimmed?: boolean;
  key: string;
}

/**
 * The parts of a bar over `spans`: each deployment segment, then hatched use outside any
 * deployment, or by machine, each machine's whole use.
 */
function barParts(
  segmentBy: MemorySegmentBy,
  spans: readonly MemorySpan[],
  highlighted?: string,
): BarPart[] {
  if (segmentBy === "machine") {
    return spans.map((span) => ({
      bytes: span.usedBytes,
      colorKey: span.machine.machineId,
      detail: <MachineDetail span={span} />,
      key: span.machine.machineId,
    }));
  }

  const segments: BarPart[] = spans
    .flatMap((span) => span.segments)
    .map((segment) => ({
      bytes: segment.bytes,
      colorKey: segment.colorKey,
      deploymentId: segment.deploymentId,
      detail: <SegmentDetail parts={spans} segment={segment} />,
      dimmed: highlighted !== undefined && highlighted !== segment.deploymentId,
      key: `${segment.machineId}:${segment.owner ?? "model"}:${segment.deploymentId}`,
    }));
  const otherBytes = sum(spans.map((span) => span.otherBytes));

  return [...segments, { bytes: otherBytes, detail: <OtherDetail parts={spans} />, key: "other" }];
}

/** A machine's part of a bar: its use, then each deployment and other use within it. */
function MachineDetail({ span }: { span: MemorySpan }) {
  return (
    <>
      <span className="font-semibold">{span.machine.name}</span>
      <span>
        {formatBytes(span.usedBytes)} / {formatBytes(span.totalBytes)}
      </span>
      {span.segments
        .filter((segment) => segment.bytes > 0n)
        .map((segment) => (
          <span key={`${segment.owner ?? "model"}:${segment.deploymentId}`}>
            {segment.name} · {formatBytes(segment.bytes)}
            {sourceSuffix(segment)}
          </span>
        ))}
      {span.otherBytes > 0n ? (
        <span>
          {span.otherLabel} · {formatBytes(span.otherBytes)}
        </span>
      ) : null}
    </>
  );
}

// Hatched neutral fill for memory no listed deployment accounts for, distinct from every deployment colour.
const otherClass =
  "bg-[repeating-linear-gradient(135deg,var(--muted-foreground)_0_3px,transparent_3px_6px)] opacity-60";
