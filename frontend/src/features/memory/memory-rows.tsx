import { type ReactNode, useId, useRef, useState } from "react";

import {
  MetricRow,
  MetricSpans,
  PendingMetricRow,
  PendingNamesLine,
} from "@/components/metrics/metric-row/metric-row";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatGib } from "@/lib/format";
import { cn } from "@/lib/utils";

import { MemoryBar } from "./memory-bar";
import { DetailLines, RowBreakdown } from "./memory-row-detail";
import {
  capacityText,
  describeLines,
  focusDetailLine,
  focusMissingReason,
  kindLabels,
  type MemoryFocusStatus,
  type MemoryRowCapacity,
  type MemorySegmentBy,
  rowDetailLines,
} from "./memory-text";
import {
  type MemoryKind,
  type MemoryRow,
  type MemorySpan,
  type UnavailableMemory,
} from "./site-memory";

export type { MemoryFocusStatus, MemoryRowCapacity, MemorySegmentBy };

export type MemoryRowsMode = "merged" | "spans";

/**
 * Memory rows as `label | bar | readout` lines: deployment segments in their card colors, hatched
 * memory in use outside any deployment, and a plain track for free memory. Values stay off the bars; hovering a
 * segment or a readout, or focusing a readout, gives the detail, which also describes the readout.
 * Clicking a row or pressing its readout opens a breakdown of the row detail and every drawn part.
 * `merged` draws each row as one bar; `spans` keeps each machine's share, named under the bar with
 * the machine's marker.
 * `focusDeploymentId` grays every other model and reads out that deployment's own usage.
 * `capacity` marks each row its verdict puts past a line: beyond the total, a line at the total
 * and a ▲ readout in destructive text; within the headroom, a line at the fit line and a readout
 * in warning text. Split into spans, each machine past its own total is marked at its total.
 * `pending` kinds without a row yet draw as empty rows, holding their
 * place while memory is read.
 */
export function MemoryRows({
  capacity,
  deviceLabel,
  focusDeploymentId,
  focusStatus,
  mode = "merged",
  pending = [],
  rows,
  surface = "page",
  unavailable = [],
}: {
  /**
   * The rows past a line, by memory kind, from a fit verdict. Unmarked by default, since measured
   * rows can draw estimates above capacity without the memory being exhausted.
   */
  capacity?: Partial<Record<MemoryKind, MemoryRowCapacity>>;
  /** Names the single device the rows describe, so its bars are distinguishable from its peers'. */
  deviceLabel?: string;
  focusDeploymentId?: string;
  /** The focused deployment's status per memory kind, for rows where its size isn't drawn. */
  focusStatus?: Partial<Record<MemoryKind, MemoryFocusStatus>>;
  mode?: MemoryRowsMode;
  /** Memory kinds whose rows are not known yet, drawn empty after the known rows. */
  pending?: readonly MemoryKind[];
  rows: readonly MemoryRow[];
  /** The background the bars sit on, which their segment separators match. */
  surface?: "page" | "panel";
  unavailable?: readonly UnavailableMemory[];
}) {
  const renderedKinds = new Set(rows.map((row) => row.kind));

  return (
    <div
      className="grid grid-cols-[auto_minmax(0,1fr)_auto] items-start gap-x-3 gap-y-2.5 text-xs"
      data-slot="memory-rows"
    >
      {rows.map((row, index) => {
        // Pools without a row of their own, such as unreadable VRAM, surface on the last row.
        const rowUnavailable = unavailable.filter(
          (entry) =>
            entry.kind === undefined ||
            entry.kind === row.kind ||
            (!renderedKinds.has(entry.kind) && index === rows.length - 1),
        );
        const stale = row.spans.some((span) => span.stale);
        return (
          <MemoryRowLine
            capacity={capacity?.[row.kind]}
            deviceLabel={deviceLabel}
            highlighted={focusDeploymentId}
            focusDeploymentId={focusDeploymentId}
            focusStatus={focusDeploymentId ? focusStatus?.[row.kind] : undefined}
            key={row.kind}
            mode={mode}
            row={row}
            stale={stale}
            surface={surface}
            unavailable={rowUnavailable}
          />
        );
      })}
      {pending
        .filter((kind) => !renderedKinds.has(kind))
        .map((kind) => (
          <PendingMemoryRowLine key={kind} kind={kind} names={mode === "spans"} />
        ))}
    </div>
  );
}

/**
 * Memory rows drawn as expandable metric rows, for a list that mixes memory with other metrics:
 * the same bar, readout and hover detail as `MemoryRows`, but a click expands the row to its
 * history instead of opening a popover. Place them inside `MetricRows`. `mode` and `surface` work
 * as in `MemoryRows`, and so does `capacity`. `freeByMachine` states free memory per machine only, never summed across
 * machines, since free memory spread over machines is not room for one workload. `pending` kinds
 * without a row yet draw as empty metric rows. `kinds` are always drawn, first and in that order,
 * so the list keeps its shape whatever memory is read; one with no row draws empty and expands to
 * `emptyChildren`. `segmentBy` draws deployments by default; by machine, each machine's use is one
 * segment in its marker color, naming its deployments on hover.
 */
export function MemoryMetricRows({
  capacity,
  children,
  deviceLabel,
  emptyChildren,
  expanded,
  focusDeploymentId,
  focusStatus,
  freeByMachine = false,
  kinds = [],
  label,
  mode = "merged",
  onExpandedChange,
  pending = [],
  rows,
  segmentBy = "deployment",
  surface = "panel",
  unavailable = [],
}: {
  /** The rows past a line, by memory kind, as `MemoryRows` marks them. */
  capacity?: Partial<Record<MemoryKind, MemoryRowCapacity>>;
  /** The expanded body of a row; a row without one doesn't expand. */
  children: (row: MemoryRow) => ReactNode;
  deviceLabel?: string;
  /** The expanded body of an empty row of `kinds`; without it, empty rows don't expand. */
  emptyChildren?: (kind: MemoryKind) => ReactNode;
  expanded: (kind: MemoryKind) => boolean;
  focusDeploymentId?: string;
  focusStatus?: Partial<Record<MemoryKind, MemoryFocusStatus>>;
  freeByMachine?: boolean;
  /** Memory kinds always drawn, ahead of other rows and in this order, empty when not read. */
  kinds?: readonly MemoryKind[];
  /** The row's label; the memory kind by default. */
  label?: (row: MemoryRow) => string;
  mode?: MemoryRowsMode;
  onExpandedChange: (kind: MemoryKind, expanded: boolean) => void;
  /** Memory kinds whose rows are not known yet, drawn empty after the known rows. */
  pending?: readonly MemoryKind[];
  rows: readonly MemoryRow[];
  segmentBy?: MemorySegmentBy;
  surface?: "page" | "panel";
  unavailable?: readonly UnavailableMemory[];
}) {
  const renderedKinds = new Set(rows.map((row) => row.kind));

  const known = rows.map((row, index) => {
    // Pools without a row of their own, such as unreadable VRAM, surface on the last row.
    const rowUnavailable = unavailable.filter(
      (entry) =>
        entry.kind === undefined ||
        entry.kind === row.kind ||
        (!renderedKinds.has(entry.kind) && index === rows.length - 1),
    );

    return (
      <MemoryMetricRow
        capacity={capacity?.[row.kind]}
        deviceLabel={deviceLabel}
        expanded={expanded(row.kind)}
        focusDeploymentId={focusDeploymentId}
        focusStatus={focusDeploymentId ? focusStatus?.[row.kind] : undefined}
        freeByMachine={freeByMachine}
        highlighted={focusDeploymentId}
        key={row.kind}
        label={label?.(row) ?? kindLabels[row.kind]}
        mode={mode}
        onExpandedChange={(open) => onExpandedChange(row.kind, open)}
        row={row}
        segmentBy={segmentBy}
        surface={surface}
        unavailable={rowUnavailable}
      >
        {children(row)}
      </MemoryMetricRow>
    );
  });

  const knownOf = (kind: MemoryKind) => known[rows.findIndex((row) => row.kind === kind)];

  return (
    <>
      {kinds.map(
        (kind) =>
          knownOf(kind) ?? (
            <PendingMetricRow
              expanded={expanded(kind)}
              key={kind}
              label={kindLabels[kind]}
              names={mode === "spans"}
              onExpandedChange={(open) => onExpandedChange(kind, open)}
            >
              {emptyChildren?.(kind)}
            </PendingMetricRow>
          ),
      )}
      {known.filter((_, index) => !kinds.includes(rows[index].kind))}
      {pending
        .filter((kind) => !renderedKinds.has(kind) && !kinds.includes(kind))
        .map((kind) => (
          <PendingMetricRow key={kind} label={kindLabels[kind]} names={mode === "spans"} />
        ))}
    </>
  );
}

/** A `MemoryRows` line whose memory is not known yet: its label, an empty track, and "—". */
function PendingMemoryRowLine({ kind, names }: { kind: MemoryKind; names: boolean }) {
  return (
    <div className="col-span-3 grid grid-cols-subgrid items-start" data-slot="memory-row">
      <span className="flex h-2.5 items-center text-muted-foreground">{kindLabels[kind]}</span>
      <div className="min-w-0">
        <div aria-hidden="true" className="h-2.5 rounded-full bg-foreground/10" />
        {names ? <PendingNamesLine /> : null}
      </div>
      <span className="flex h-2.5 items-center justify-end text-muted-foreground tabular-nums">
        —
      </span>
    </div>
  );
}

function MemoryRowLine({
  capacity,
  deviceLabel,
  focusDeploymentId,
  focusStatus,
  highlighted,
  mode,
  row,
  stale,
  surface,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  deviceLabel?: string;
  focusDeploymentId?: string;
  focusStatus?: MemoryFocusStatus;
  highlighted?: string;
  mode: MemoryRowsMode;
  row: MemoryRow;
  stale: boolean;
  surface: "page" | "panel";
  unavailable: readonly UnavailableMemory[];
}) {
  const descriptionId = useId();
  const { detail, flagged, readoutLabel, readout } = rowPresentation({
    capacity,
    focusDeploymentId,
    focusStatus,
    row,
    stale,
    unavailable,
  });
  const staleClass = cn(stale && "opacity-60");
  const [breakdownOpen, setBreakdownOpen] = useState(false);
  const rowRef = useRef<HTMLDivElement>(null);
  const readoutRef = useRef<HTMLButtonElement>(null);

  return (
    // Tooltips never open on touch, so a click anywhere on the row toggles its breakdown. The
    // readout button is the keyboard path to the same breakdown. The pseudo-element stretches the
    // click area over half the row gap on each side, behind the row's own content.
    // eslint-disable-next-line jsx-a11y/click-events-have-key-events, jsx-a11y/no-static-element-interactions
    <div
      className="relative isolate col-span-3 grid grid-cols-subgrid items-start after:absolute after:inset-x-0 after:-inset-y-1.25 after:-z-1"
      data-slot="memory-row"
      onClick={(event) => {
        // Clicks inside the portaled breakdown bubble here through React, so only the row's own
        // elements count, and the readout toggles the breakdown itself.
        const target = event.target as Node;
        if (!event.currentTarget.contains(target) || readoutRef.current?.contains(target)) return;
        setBreakdownOpen((open) => !open);
      }}
      ref={rowRef}
    >
      <span className={cn("flex h-2.5 items-center text-muted-foreground", staleClass)}>
        {kindLabels[row.kind]}
      </span>
      <div className={cn("min-w-0", staleClass)} data-stale={stale}>
        {mode === "spans" ? (
          <MemorySpans
            capacity={capacity}
            deviceLabel={deviceLabel}
            highlighted={highlighted}
            row={row}
            segmentBy="deployment"
            surface={surface}
            unavailable={unavailable}
          />
        ) : (
          <MemoryBar
            capacity={capacity}
            deviceLabel={deviceLabel}
            highlighted={highlighted}
            kind={row.kind}
            segmentBy="deployment"
            spans={row.spans}
            surface={surface}
            totalBytes={row.totalBytes}
            unavailable={unavailable}
          />
        )}
        {/* The tooltip mounts only while open, so this copy of its detail describes the readout.
            It sits in the bar's cell to keep three cells per row. */}
        <span hidden id={descriptionId}>
          {describeLines(detail)}
        </span>
      </div>
      <Popover
        onOpenChange={(open, details) => {
          // A press on the row is handled by the row's own click, so it can close the breakdown.
          if (
            details.reason === "outside-press" &&
            rowRef.current?.contains(details.event.target as Node)
          ) {
            return;
          }
          setBreakdownOpen(open);
        }}
        open={breakdownOpen}
      >
        <Tooltip>
          <TooltipTrigger
            aria-describedby={descriptionId}
            aria-label={readoutLabel}
            className={cn(
              "flex h-2.5 items-center justify-end rounded-sm bg-transparent p-0 whitespace-nowrap tabular-nums outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
              flagged === undefined
                ? "text-muted-foreground"
                : flagged.beyondTotal
                  ? "text-destructive"
                  : "text-warning-foreground",
              staleClass,
            )}
            ref={readoutRef}
            render={<PopoverTrigger render={<button type="button" />} />}
          >
            {flagged?.beyondTotal ? <span aria-hidden="true">▲&nbsp;</span> : null}
            {readout}
          </TooltipTrigger>
          <TooltipContent className="flex-col items-start gap-0.5 text-left">
            <DetailLines lines={detail} />
          </TooltipContent>
        </Tooltip>
        <PopoverContent
          align="end"
          anchor={rowRef}
          aria-label={`${kindLabels[row.kind]} memory breakdown`}
          className="w-auto max-w-[min(var(--container-xs),var(--available-width))] gap-2 text-xs"
        >
          <RowBreakdown detail={detail} row={row} />
        </PopoverContent>
      </Popover>
    </div>
  );
}

function MemoryMetricRow({
  capacity,
  children,
  deviceLabel,
  expanded,
  focusDeploymentId,
  focusStatus,
  freeByMachine,
  highlighted,
  label,
  mode,
  onExpandedChange,
  row,
  segmentBy,
  surface,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  children: ReactNode;
  deviceLabel?: string;
  expanded: boolean;
  focusDeploymentId?: string;
  focusStatus?: MemoryFocusStatus;
  freeByMachine: boolean;
  highlighted?: string;
  label: string;
  mode: MemoryRowsMode;
  onExpandedChange: (expanded: boolean) => void;
  row: MemoryRow;
  segmentBy: MemorySegmentBy;
  surface: "page" | "panel";
  unavailable: readonly UnavailableMemory[];
}) {
  const descriptionId = useId();
  const stale = row.spans.some((span) => span.stale);
  const { detail, flagged, readout, readoutLabel } = rowPresentation({
    capacity,
    focusDeploymentId,
    focusStatus,
    freeByMachine,
    row,
    stale,
    unavailable,
  });
  const staleClass = cn(stale && "opacity-60");

  return (
    <MetricRow
      bar={
        <div className={staleClass} data-stale={stale}>
          {mode === "spans" ? (
            <MemorySpans
              capacity={capacity}
              deviceLabel={deviceLabel}
              highlighted={highlighted}
              row={row}
              segmentBy={segmentBy}
              surface={surface}
              unavailable={unavailable}
            />
          ) : (
            <MemoryBar
              capacity={capacity}
              deviceLabel={deviceLabel}
              highlighted={highlighted}
              kind={row.kind}
              segmentBy={segmentBy}
              spans={row.spans}
              surface={surface}
              totalBytes={row.totalBytes}
              unavailable={unavailable}
            />
          )}
          {/* The tooltip mounts only while open, so this copy of its detail describes the readout. */}
          <span hidden id={descriptionId}>
            {describeLines(detail)}
          </span>
        </div>
      }
      expanded={expanded}
      label={label}
      onExpandedChange={onExpandedChange}
      value={
        <Tooltip>
          <TooltipTrigger
            aria-describedby={descriptionId}
            aria-label={readoutLabel}
            className={cn(
              "rounded-sm bg-transparent p-0 whitespace-nowrap tabular-nums outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
              flagged === undefined
                ? undefined
                : flagged.beyondTotal
                  ? "text-destructive"
                  : "text-warning-foreground",
              staleClass,
            )}
            render={<button type="button" />}
          >
            {flagged?.beyondTotal ? <span aria-hidden="true">▲&nbsp;</span> : null}
            {readout}
          </TooltipTrigger>
          <TooltipContent className="flex-col items-start gap-0.5 text-left">
            <DetailLines lines={detail} />
          </TooltipContent>
        </Tooltip>
      }
    >
      {children}
    </MetricRow>
  );
}

/**
 * A row split into one bar per machine, each sized by its capacity and named with its marker. A
 * row marked beyond its total marks each machine past its own total.
 */
function MemorySpans({
  capacity,
  deviceLabel,
  highlighted,
  row,
  segmentBy,
  surface,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  deviceLabel?: string;
  highlighted?: string;
  row: MemoryRow;
  segmentBy: MemorySegmentBy;
  surface: "page" | "panel";
  unavailable: readonly UnavailableMemory[];
}) {
  const spanCapacity = (span: MemorySpan): MemoryRowCapacity | undefined =>
    capacity?.beyondTotal && span.overTotalBytes > 0n
      ? {
          beyondTotal: true,
          headroomPercent: 0,
          lineBytes: span.totalBytes,
          overBytes: span.overTotalBytes,
        }
      : undefined;

  return (
    <MetricSpans
      spans={row.spans.map((span) => ({
        bar: (
          <MemoryBar
            capacity={spanCapacity(span)}
            deviceLabel={deviceLabel}
            highlighted={highlighted}
            kind={row.kind}
            segmentBy={segmentBy}
            spans={[span]}
            surface={surface}
            totalBytes={span.totalBytes}
            unavailable={unavailable}
          />
        ),
        colorKey: span.machine.machineId,
        key: span.machine.machineId,
        name: span.machine.name,
        weight: Number(span.totalBytes),
      }))}
    />
  );
}

/**
 * What a memory row reads out: the readout text and its accessible name, the detail lines, and
 * the capacity verdict its readout flags.
 */
function rowPresentation({
  capacity,
  focusDeploymentId,
  focusStatus,
  freeByMachine = false,
  row,
  stale,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  focusDeploymentId?: string;
  focusStatus?: MemoryFocusStatus;
  freeByMachine?: boolean;
  row: MemoryRow;
  stale: boolean;
  unavailable: readonly UnavailableMemory[];
}) {
  const segments = row.spans.flatMap((span) => span.segments);
  const unmeasured = row.spans.flatMap((span) => span.unmeasured);
  const focused = focusDeploymentId
    ? segments.find((segment) => segment.deploymentId === focusDeploymentId)
    : undefined;
  const focusedUnmeasured = focusDeploymentId
    ? unmeasured.find((usage) => usage.deploymentId === focusDeploymentId)
    : undefined;
  const readout =
    focusStatus?.readout ??
    `${focusDeploymentId ? (focused ? formatGib(focused.bytes) : "—") : formatGib(row.usedBytes)} / ${formatGib(row.totalBytes)} GiB`;
  const missingReason =
    focusDeploymentId && !focused && !focusStatus
      ? focusMissingReason(row.kind, focusedUnmeasured)
      : undefined;
  // A status readout says nothing about capacity, so only the bar's marker remains.
  const flagged = focusStatus?.readout ? undefined : capacity;
  const detail = [
    ...(focusDeploymentId ? [focusDetailLine(focused, missingReason, focusStatus)] : []),
    ...rowDetailLines({ capacity, freeByMachine, row, stale, unavailable, unmeasured }),
  ];
  const readoutLabel = `${kindLabels[row.kind]} ${readout}${missingReason ? `, this model: ${missingReason}` : ""}${flagged ? `, ${capacityText(flagged)}` : ""}`;

  return { detail, flagged, readout, readoutLabel };
}
