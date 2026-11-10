import { formatBytes, formatRelativeTime } from "@/lib/format";

import {
  type MemoryKind,
  type MemoryRow,
  type MemorySegment,
  type MemorySpan,
  type UnavailableMemory,
  type UnmeasuredUsage,
} from "./site-memory";

/**
 * What a bar's segments stand for: each deployment in its card color, with hatched use outside any
 * deployment, or each machine's whole use in its marker color.
 */
export type MemorySegmentBy = "deployment" | "machine";

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

/** One line of a row's detail, shown in its tooltip and describing its readout. */
export interface DetailLine {
  indent?: boolean;
  key: string;
  strong?: boolean;
  text: string;
}

/** The focused deployment's line: its status, its drawn size, or why it has none. */
export function focusDetailLine(
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
export function rowDetailLines({
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
export function describeLines(lines: readonly DetailLine[]): string {
  return lines.map((line) => (/[.!?…]$/.test(line.text) ? line.text : `${line.text}.`)).join(" ");
}

/** Accessible bar summary, naming estimates, what the bar can't draw, and any marked overrun. */
export function barLabel(
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
export function capacityText(capacity: MemoryRowCapacity): string {
  const bytes = formatBytes(capacity.overBytes);
  return capacity.beyondTotal
    ? `over capacity by ${bytes}`
    : `less than ${capacity.headroomPercent}% free by ${bytes}`;
}

export const kindLabels: Record<MemoryKind, string> = { ram: "RAM", vram: "VRAM" };

/** What a row's memory is. Unified machines' GPUs allocate from system memory, so it reads as RAM. */
function rowDescription(row: MemoryRow): string {
  return row.kind === "vram" ? "VRAM: dedicated GPU memory" : "RAM: system memory";
}

/**
 * Why the focused deployment has no readout on a row. A dedicated machine's host RAM leaves out
 * models that hold no host memory beyond the system's, so they aren't listed as unsized there.
 */
export function focusMissingReason(kind: MemoryKind, unmeasured?: UnmeasuredUsage): string {
  if (unmeasured) return `not measured, ${reasonText(unmeasured)}`;
  return kind === "ram" ? "host memory isn't estimated or measured" : "not measured here";
}

export function reasonText(usage: UnmeasuredUsage): string {
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

/**
 * Names a drawn size that is a catalog estimate, a starting deployment's expected use or a
 * prediction rather than memory observed for the model.
 */
export function sourceSuffix(segment: MemorySegment): string {
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

function freeBytes(used: bigint, total: bigint): bigint {
  return total > used ? total - used : 0n;
}

export function sum(values: readonly bigint[]): bigint {
  return values.reduce((total, value) => total + value, 0n);
}
