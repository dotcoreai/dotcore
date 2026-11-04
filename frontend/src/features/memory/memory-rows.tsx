import { type ReactNode, useEffect, useId, useRef, useState } from "react";

import { useResourceColors } from "@/app/resource-color-context";
import {
  MetricRow,
  MetricSpans,
  PendingMetricRow,
  PendingNamesLine,
} from "@/components/metrics/metric-row/metric-row";
import { Popover, PopoverContent, PopoverTrigger } from "@/components/ui/popover";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { formatBytes, formatGib, formatRelativeTime } from "@/lib/format";
import { cn } from "@/lib/utils";

import { useHoveredDeployment, useMemoryHoverSetter } from "./memory-hover-store";
import {
  type MemoryKind,
  type MemoryRow,
  type MemorySegment,
  type MemorySpan,
  type UnavailableMemory,
  type UnmeasuredUsage,
} from "./site-memory";

export type MemoryRowsMode = "merged" | "spans";

/**
 * What the rows say about the focused deployment on one row while its size can't be drawn, such as
 * a pending estimate: `detail` is its line in the row's detail, and `readout` replaces the readout.
 */
export interface MemoryFocusStatus {
  detail: string;
  readout?: string;
}

/**
 * Where a row's use stands against the line its fit verdict judges, as the capacity component
 * states it: beyond the row's total, or past the fit line into the headroom kept free under it.
 */
export interface MemoryRowCapacity {
  /** Whether use is beyond the row's total, not only past its fit line. */
  beyondTotal: boolean;
  /** Share of the total, in whole percent, that the fit line keeps free. */
  headroomPercent: number;
  /** The line the bar marks: the total beyond it, the fit line within the headroom. */
  lineBytes: bigint;
  /** Use past that line. */
  overBytes: bigint;
}

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
  const hovered = useHoveredDeployment();
  const setHovered = useMemoryHoverSetter();
  const highlighted = focusDeploymentId ?? hovered;
  const renderedKinds = new Set(rows.map((row) => row.kind));

  // A hovered segment that unmounts never sends its pointer leave, so the link clears here.
  useEffect(() => () => setHovered(undefined), [setHovered]);

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
            highlighted={highlighted}
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
 * `emptyChildren`.
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
  surface?: "page" | "panel";
  unavailable?: readonly UnavailableMemory[];
}) {
  const hovered = useHoveredDeployment();
  const setHovered = useMemoryHoverSetter();
  const highlighted = focusDeploymentId ?? hovered;
  const renderedKinds = new Set(rows.map((row) => row.kind));

  // A hovered segment that unmounts never sends its pointer leave, so the link clears here.
  useEffect(() => () => setHovered(undefined), [setHovered]);

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
        highlighted={highlighted}
        key={row.kind}
        label={label?.(row) ?? kindLabels[row.kind]}
        mode={mode}
        onExpandedChange={(open) => onExpandedChange(row.kind, open)}
        row={row}
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
  const segments = row.spans.flatMap((span) => span.segments);
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
            surface={surface}
            unavailable={unavailable}
          />
        ) : (
          <Bar
            ariaLabel={barLabel(row.kind, row.spans, unavailable, capacity, deviceLabel)}
            capacity={capacity}
            highlighted={highlighted}
            otherParts={row.spans}
            segments={segments}
            surface={surface}
            totalBytes={row.totalBytes}
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
              surface={surface}
              unavailable={unavailable}
            />
          ) : (
            <Bar
              ariaLabel={barLabel(row.kind, row.spans, unavailable, capacity, deviceLabel)}
              capacity={capacity}
              highlighted={highlighted}
              otherParts={row.spans}
              segments={row.spans.flatMap((span) => span.segments)}
              surface={surface}
              totalBytes={row.totalBytes}
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
  surface,
  unavailable,
}: {
  capacity?: MemoryRowCapacity;
  deviceLabel?: string;
  highlighted?: string;
  row: MemoryRow;
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
          <Bar
            ariaLabel={barLabel(row.kind, [span], unavailable, spanCapacity(span), deviceLabel)}
            capacity={spanCapacity(span)}
            highlighted={highlighted}
            otherParts={[span]}
            segments={span.segments}
            surface={surface}
            totalBytes={span.totalBytes}
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

/** A row's breakdown for a click or tap: the row detail, then every drawn part of its bar. */
function RowBreakdown({ detail, row }: { detail: readonly DetailLine[]; row: MemoryRow }) {
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
          <span aria-hidden="true" className={cn("mt-1 size-2 rounded-xs", otherClass)} />
          <OtherDetail parts={row.spans} />
        </div>
      ) : null}
    </>
  );
}

function DetailLines({ lines }: { lines: readonly DetailLine[] }) {
  return lines.map((line) => (
    <span className={cn(line.indent && "pl-3", line.strong && "font-semibold")} key={line.key}>
      {line.text}
    </span>
  ));
}

/**
 * One bar: segments, then hatched memory in use outside any deployment, over a free-memory track. It
 * scales to its contents when drawn use exceeds the total, such as a claim past it, so it never
 * overflows. With a
 * `capacity` verdict, a line shows where the total ends or where the fit line sits.
 */
function Bar({
  ariaLabel,
  capacity,
  highlighted,
  otherParts,
  segments,
  surface,
  totalBytes,
}: {
  ariaLabel: string;
  capacity?: MemoryRowCapacity;
  highlighted?: string;
  otherParts: readonly MemorySpan[];
  segments: readonly MemorySegment[];
  surface: "page" | "panel";
  totalBytes: bigint;
}) {
  const setHovered = useMemoryHoverSetter();
  const { colorOf } = useResourceColors();
  const otherBytes = sum(otherParts.map((span) => span.otherBytes));
  const drawn = sum(segments.map((segment) => segment.bytes)) + otherBytes;
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
        {/* A zero-byte segment would still draw its separator, so it stays in the text only. */}
        {segments
          .filter((segment) => segment.bytes > 0n)
          .map((segment) => (
            <Tooltip
              key={`${segment.machineId}:${segment.owner ?? "model"}:${segment.deploymentId}`}
            >
              <TooltipTrigger
                className={cn(
                  "h-full shrink-0 transition-[filter,opacity]",
                  highlighted !== undefined &&
                    highlighted !== segment.deploymentId &&
                    "opacity-45 grayscale",
                )}
                data-deployment-id={segment.deploymentId}
                onPointerEnter={() =>
                  setHovered(segment.owner === "application" ? undefined : segment.deploymentId)
                }
                onPointerLeave={() => setHovered(undefined)}
                render={<span />}
                style={{
                  backgroundColor: colorOf(segment.colorKey),
                  width: width(segment.bytes),
                }}
              />
              <TooltipContent className="flex-col items-start gap-0.5 text-left">
                <SegmentDetail parts={otherParts} segment={segment} />
              </TooltipContent>
            </Tooltip>
          ))}
        {otherBytes > 0n ? (
          <Tooltip>
            <TooltipTrigger
              className={cn("h-full shrink-0", otherClass)}
              render={<span />}
              style={{ width: width(otherBytes) }}
            />
            <TooltipContent className="flex-col items-start gap-0.5 text-left">
              <OtherDetail parts={otherParts} />
            </TooltipContent>
          </Tooltip>
        ) : null}
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

function SegmentDetail({
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

function OtherDetail({ parts }: { parts: readonly MemorySpan[] }) {
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

/** One line of a row's detail, shown in its tooltip and describing its readout. */
interface DetailLine {
  indent?: boolean;
  key: string;
  strong?: boolean;
  text: string;
}

/** The focused deployment's line: its status, its drawn size, or why it has none. */
function focusDetailLine(
  focused: MemorySegment | undefined,
  missingReason: string | undefined,
  status: MemoryFocusStatus | undefined,
): DetailLine {
  const line = { key: "focus", strong: true };
  if (status) return { ...line, text: status.detail };
  if (focused) {
    const subject = focused.source === "predicted" ? "This deployment" : "This model";
    return {
      ...line,
      text: `${subject}: ${formatBytes(focused.bytes)}${sourceSuffix(focused)}`,
    };
  }

  return { ...line, text: `This model: ${missingReason}` };
}

/**
 * Row totals, after deploy when a prediction is drawn, the per-machine and per-GPU split,
 * freshness, and what the bar can't draw.
 */
function rowDetailLines({
  capacity,
  freeByMachine,
  row,
  stale,
  unavailable,
  unmeasured,
}: {
  capacity?: MemoryRowCapacity;
  freeByMachine: boolean;
  row: MemoryRow;
  stale: boolean;
  unavailable: readonly UnavailableMemory[];
  unmeasured: readonly UnmeasuredUsage[];
}): DetailLine[] {
  const drawn = row.usedBytes;
  const segments = row.spans.flatMap((span) => span.segments);
  const estimated = segments.filter((segment) => segment.source === "estimated");
  const predicted = segments.some(
    (segment) => segment.source === "predicted" && segment.bytes > 0n,
  );
  const observedAt = row.spans
    .map((span) => span.observedAt)
    .filter((value) => value !== undefined)
    .sort((left, right) => Number(left.seconds - right.seconds))[0];
  // Free memory spread over machines is not room for one workload, so it is stated per machine.
  const freePerMachine = freeByMachine && row.spans.length > 1;
  const lines: DetailLine[] = [
    { key: "title", strong: true, text: rowDescription(row) },
    {
      key: "totals",
      text: `${formatBytes(drawn)} used${predicted ? " after deploy" : ""}${
        capacity
          ? capacity.beyondTotal
            ? ` · ${formatBytes(capacity.overBytes)} over capacity`
            : ` · less than ${capacity.headroomPercent}% free`
          : freePerMachine
            ? ""
            : ` · ${formatBytes(freeBytes(drawn, row.totalBytes))} free`
      }`,
    },
  ];
  if (row.spans.length > 1 || row.spans.some((span) => span.devices.length > 1)) {
    for (const span of row.spans) {
      lines.push({
        key: `span:${span.machine.machineId}`,
        text: `${span.machine.name}: ${formatBytes(span.usedBytes)} / ${formatBytes(span.totalBytes)}${
          freePerMachine ? ` · ${formatBytes(freeBytes(span.usedBytes, span.totalBytes))} free` : ""
        }`,
      });
      if (span.devices.length > 1) {
        for (const device of span.devices) {
          lines.push({
            indent: true,
            key: `device:${span.machine.machineId}:${device.label}`,
            text: `${device.label}: ${formatBytes(device.usedBytes)} / ${formatBytes(device.totalBytes)}`,
          });
        }
      }
    }
  }
  if (estimated.length) {
    lines.push({
      key: "estimated",
      text: `Estimated: ${estimated.map((segment) => segment.name).join(", ")}`,
    });
  }
  for (const usage of unmeasured) {
    lines.push({
      key: `unmeasured:${usage.owner ?? "model"}:${usage.deploymentId}`,
      text: `Not measured: ${usage.name} (${reasonText(usage)})`,
    });
  }
  for (const entry of unavailable) {
    lines.push({
      key: `unavailable:${entry.machine.machineId}:${entry.kind ?? "machine"}`,
      text: `Unreadable: ${entry.machine.name}${entry.kind ? ` ${kindLabels[entry.kind]}` : ""}${
        entry.deployments.length ? ` (${entry.deployments.map((ref) => ref.name).join(", ")})` : ""
      }`,
    });
  }
  if (observedAt) {
    lines.push({ key: "observed", text: `Observed ${formatRelativeTime(observedAt).text}` });
  }
  if (stale) lines.push({ key: "stale", text: "Latest update failed; values may be outdated." });

  return lines;
}

/** Detail lines as one description, each closed as a sentence. */
function describeLines(lines: readonly DetailLine[]): string {
  return lines.map((line) => (/[.!?…]$/.test(line.text) ? line.text : `${line.text}.`)).join(" ");
}

/**
 * Names a drawn size that is a catalog estimate, a starting deployment's expected use or a
 * prediction rather than memory observed for the model.
 */
function sourceSuffix(segment: MemorySegment): string {
  switch (segment.source) {
    case "claimed":
      return " (starting)";
    case "estimated":
      return " (estimated)";
    case "predicted":
      return " (predicted)";
    case "observed":
      return "";
  }
}

/** Accessible bar summary, naming estimates, what the bar can't draw, and any marked overrun. */
function barLabel(
  kind: MemoryKind,
  spans: readonly MemorySpan[],
  unavailable: readonly UnavailableMemory[],
  capacity: MemoryRowCapacity | undefined,
  deviceLabel?: string,
): string {
  const used = sum(spans.map((span) => span.usedBytes));
  const total = sum(spans.map((span) => span.totalBytes));
  const machines =
    spans.length === 1
      ? ` on ${spans[0].machine.name}${deviceLabel ? ` (${deviceLabel})` : ""}`
      : "";
  const parts = [
    `${kindLabels[kind]}${machines}: ${formatBytes(used)} used of ${formatBytes(total)}`,
  ];
  const segments = spans.flatMap((span) => span.segments);
  const estimated = segments.filter((segment) => segment.source === "estimated");
  const unmeasured = spans.flatMap((span) => span.unmeasured);

  if (segments.length) {
    parts.push(
      `deployments: ${segments
        .map((segment) => `${segment.name} ${formatBytes(segment.bytes)}`)
        .join(", ")}`,
    );
  }
  if (estimated.length)
    parts.push(`estimated: ${estimated.map((segment) => segment.name).join(", ")}`);
  if (unmeasured.length) {
    parts.push(
      `not measured: ${unmeasured.map((usage) => `${usage.name} (${reasonText(usage)})`).join(", ")}`,
    );
  }
  if (unavailable.length) {
    parts.push(
      `unreadable: ${unavailable
        .map(
          (entry) =>
            `${entry.machine.name}${entry.kind ? ` ${kindLabels[entry.kind]}` : ""}${
              entry.deployments.length
                ? ` (${entry.deployments.map((ref) => ref.name).join(", ")})`
                : ""
            }`,
        )
        .join(", ")}`,
    );
  }
  if (capacity) parts.push(capacityText(capacity));

  return parts.join("; ");
}

/** A capacity verdict in a few words, such as "over capacity by 5 GiB". */
function capacityText(capacity: MemoryRowCapacity): string {
  const bytes = formatBytes(capacity.overBytes);
  return capacity.beyondTotal
    ? `over capacity by ${bytes}`
    : `less than ${capacity.headroomPercent}% free by ${bytes}`;
}

const kindLabels: Record<MemoryKind, string> = { ram: "RAM", vram: "VRAM" };

/** What a row's memory is. Unified machines' GPUs allocate from system memory, so it reads as RAM. */
function rowDescription(row: MemoryRow): string {
  return row.kind === "vram" ? "VRAM: dedicated GPU memory" : "RAM: system memory";
}

// Hatched neutral fill for memory no listed deployment accounts for, distinct from every deployment colour.
const otherClass =
  "bg-[repeating-linear-gradient(135deg,var(--muted-foreground)_0_3px,transparent_3px_6px)] opacity-60";

// One drawn part in a row's breakdown: its swatch, then its detail lines beside it.
const breakdownEntryClass =
  "grid grid-cols-[auto_minmax(0,1fr)] gap-x-2 gap-y-0.5 border-t border-border pt-2 [&>:not(:first-child)]:col-start-2";

/**
 * Why the focused deployment has no readout on a row. A dedicated machine's host RAM leaves out
 * models that hold no host memory beyond the system's, so they aren't listed as unsized there.
 */
function focusMissingReason(kind: MemoryKind, unmeasured?: UnmeasuredUsage): string {
  if (unmeasured) return `not measured, ${reasonText(unmeasured)}`;
  return kind === "ram" ? "host memory isn't estimated or measured" : "not measured here";
}

function reasonText(usage: UnmeasuredUsage): string {
  switch (usage.reason) {
    case "noEstimate":
      return "no catalog sizing";
    case "notReady":
      return "not ready yet";
    case "notRunning":
      return "process not running";
    case "pending":
      return "waiting for the first reading";
    case "unavailable":
      return "measurement unavailable";
    case "stale":
      return "no current reading";
  }
}

function freeBytes(used: bigint, total: bigint): bigint {
  return total > used ? total - used : 0n;
}

function sum(values: readonly bigint[]): bigint {
  return values.reduce((total, value) => total + value, 0n);
}
