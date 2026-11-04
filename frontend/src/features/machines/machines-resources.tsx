import type { ReactNode } from "react";

import {
  MetricBar,
  MetricRow,
  MetricRows,
  MetricSpans,
  PendingMetricRow,
} from "@/components/metrics/metric-row/metric-row";
import { formatChartValue } from "@/components/metrics/time-series-chart/chart-data";
import { PageSectionHeading } from "@/components/ui/page-header";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { MemoryRowHistory } from "@/features/memory/memory-row-history";
import { MemoryMetricRows } from "@/features/memory/memory-rows";
import { MemoryViewSwitch, MemoryViewSwitchPlace } from "@/features/memory/memory-view-switch";
import { pastTotalCapacity } from "@/features/memory/pool-spans";
import type { HistoryBreakdown } from "@/features/metrics/history-series";
import { fillFraction } from "@/features/metrics/metric-current";
import { type MetricRowChart, MetricRowHistory } from "@/features/metrics/metric-row-history";
import { MemoryView } from "@/gen/dotcore/console/v1/preferences_pb";
import { formatBytes, formatCount } from "@/lib/format";
import { cn } from "@/lib/utils";

import { CurrentFailedStatus } from "./machine-metrics";
import {
  cpuHistory,
  cpuReading,
  gpuHistory,
  type GpuReading,
  gpuReading,
  type LeftOutPart,
  type ResourceReading,
  storageHistory,
  storageReading,
} from "./machine-resources";
import type { MachineResources } from "./use-machine-resources";

/**
 * The Machines page overview of its counted machines' resources: CPU, VRAM, RAM, storage and GPU
 * as metric rows that expand to their history, merged across machines or kept per machine. A value
 * that leaves machines out shows a partial state and names them on hover; machines not counted now
 * are named under the rows with their state. Every row is always drawn and expandable, empty while
 * the machines and their hardware are read or when no counted machine has that resource, so the
 * section keeps its shape.
 */
export function MachinesResources({
  machineCount,
  onViewChange,
  resources,
  view,
}: {
  /**
   * Listed machines, 0 until the list is read; the view choice is offered only with more than
   * one.
   */
  machineCount: number;
  onViewChange: (view: MemoryView) => void;
  resources: MachineResources;
  view: MemoryView;
}) {
  const { counted, history: memoryHistory, memory, panel, uncounted } = resources;
  const { described, machineNames: names } = memoryHistory;
  const byMachine = view === MemoryView.BY_MACHINE && machineCount > 1;
  const breakdown: HistoryBreakdown = byMachine ? "machines" : "combined";
  const [cpuGrid] = panel.current("cpu") ?? [];
  const [utilizationGrid, powerGrid] = panel.current("gpu") ?? [];
  const [storageGrid] = panel.current("storage") ?? [];
  const cpu = cpuReading(counted, cpuGrid);
  const gpu = gpuReading(counted, utilizationGrid, powerGrid);
  const storage = storageReading(counted, storageGrid);
  const memoryRows = memory.kind === "ready" ? memory.memory.rows : [];
  const history = (groupId: string) => panel.history(groupId) ?? [];
  // No machine is placed yet while the list or every first snapshot is still being read.
  const reading = !counted.length && !uncounted.length;
  // Before the list is read (no count yet), the chosen view decides whether rows name machines.
  const spans = reading ? view === MemoryView.BY_MACHINE && machineCount !== 1 : byMachine;
  const charts = {
    cpu: chartOf(history("cpu"), ([grid]) => ({
      grid,
      series: cpuHistory(grid, described, names, breakdown),
    })),
    gpu: chartOf(history("gpu"), ([grid]) => ({
      grid,
      series: gpuHistory(grid, names, breakdown),
    })),
    storage: chartOf(history("storage"), ([used, size]) => ({
      grid: used,
      series: storageHistory(used, size, names, breakdown),
    })),
  };
  const emptyRow = (id: keyof typeof charts, label: string, unit: "bytes" | "ratio") => (
    <PendingMetricRow
      expanded={panel.expanded(id)}
      label={label}
      names={spans}
      onExpandedChange={(expanded) => panel.setExpanded(id, expanded)}
    >
      <MetricRowHistory chart={charts[id]} label={`${label} history`} panel={panel} unit={unit} />
    </PendingMetricRow>
  );

  return (
    <section aria-labelledby="machine-resources-heading" className="flex flex-col gap-3">
      <div className="flex min-h-6 items-center justify-between gap-4">
        <PageSectionHeading id="machine-resources-heading">Resources</PageSectionHeading>
        {machineCount > 1 ? (
          <MemoryViewSwitch label="Resources view" onChange={onViewChange} view={view} />
        ) : (
          <MemoryViewSwitchPlace />
        )}
      </div>
      {counted.length || reading ? (
        <MetricRows fitLabels>
          {cpu.parts.length ? (
            <ReadingRow
              breakdown={breakdown}
              chart={charts.cpu}
              id="cpu"
              label="CPU"
              panel={panel}
              reading={cpu}
              unit="ratio"
              value={
                cpu.value === undefined
                  ? "—"
                  : `${formatChartValue("ratio", cpu.value)} · ${formatCount(cpu.capacity, "CPU")}`
              }
            />
          ) : (
            emptyRow("cpu", "CPU", "ratio")
          )}
          {gpu ? (
            <ReadingRow
              breakdown={breakdown}
              chart={charts.gpu}
              id="gpu"
              label="GPU"
              panel={panel}
              reading={gpu}
              unit="ratio"
              value={gpuValue(gpu)}
            />
          ) : (
            emptyRow("gpu", "GPU", "ratio")
          )}
          {/* RAM leads VRAM, so the rows read from host to accelerator after CPU and GPU. */}
          <MemoryMetricRows
            capacity={pastTotalCapacity(memoryRows)}
            emptyChildren={(kind) => (
              <MemoryRowHistory
                breakdown={breakdown}
                history={memoryHistory}
                kind={kind}
                panel={panel}
              />
            )}
            expanded={(kind) => panel.expanded(kind)}
            freeByMachine
            kinds={memoryKinds}
            mode={spans ? "spans" : "merged"}
            onExpandedChange={(kind, expanded) => panel.setExpanded(kind, expanded)}
            rows={memoryRows}
            surface="page"
            unavailable={memory.kind === "ready" ? memory.memory.unavailable : []}
          >
            {(row) => (
              <MemoryRowHistory
                breakdown={breakdown}
                history={memoryHistory}
                kind={row.kind}
                panel={panel}
              />
            )}
          </MemoryMetricRows>
          {storage ? (
            <ReadingRow
              breakdown={breakdown}
              chart={charts.storage}
              id="storage"
              label="Storage"
              panel={panel}
              reading={storage}
              unit="bytes"
              value={
                storage.value === undefined
                  ? "—"
                  : `${formatBytes(BigInt(Math.round(storage.value)))} / ${formatBytes(
                      BigInt(Math.round(storage.capacity)),
                    )}`
              }
            />
          ) : (
            emptyRow("storage", "Storage", "bytes")
          )}
        </MetricRows>
      ) : null}
      {uncounted.length ? (
        <p className="text-xs text-muted-foreground">
          Not counted: {uncounted.map((machine) => `${machine.name} (${machine.state})`).join(", ")}
        </p>
      ) : null}
      <CurrentFailedStatus panel={panel} />
    </section>
  );
}

/**
 * A combined reading as a metric row: one bar against its capacity, or a bar per machine in its
 * marker color. A value leaving machines out without a reading reads as partial, and its tooltip
 * names every machine left out and why.
 */
function ReadingRow({
  breakdown,
  chart,
  id,
  label,
  panel,
  reading,
  unit,
  value,
}: {
  breakdown: HistoryBreakdown;
  chart: MetricRowChart | undefined;
  id: string;
  label: string;
  panel: MachineResources["panel"];
  reading: ResourceReading;
  unit: "bytes" | "ratio";
  value: string;
}) {
  const partial = reading.leftOut.some((part) => part.reason === "noReading");
  const fraction = unit === "ratio" ? reading.value : fillFraction(reading.value, reading.capacity);

  return (
    <MetricRow
      bar={
        <div className={cn(partial && "opacity-60")} data-partial={partial}>
          {breakdown === "machines" ? (
            <MetricSpans
              spans={reading.parts.map((part) => {
                const share =
                  unit === "ratio" ? part.value : fillFraction(part.value, part.capacity);
                return {
                  bar: <MetricBar color={{ resource: part.machineId }} fraction={share} />,
                  colorKey: part.machineId,
                  key: part.machineId,
                  name: part.name,
                  value: share === undefined ? "—" : formatChartValue("ratio", share),
                  weight: part.capacity,
                };
              })}
            />
          ) : (
            <MetricBar fraction={fraction} />
          )}
        </div>
      }
      expanded={panel.expanded(id)}
      label={label}
      onExpandedChange={(expanded) => panel.setExpanded(id, expanded)}
      value={<LeftOutValue leftOut={reading.leftOut} partial={partial} value={value} />}
    >
      <MetricRowHistory chart={chart} label={`${label} history`} panel={panel} unit={unit} />
    </MetricRow>
  );
}

/** A row value, with the machines its reading leaves out on hover or focus. */
function LeftOutValue({
  leftOut,
  partial,
  value,
}: {
  leftOut: readonly LeftOutPart[];
  partial: boolean;
  value: string;
}): ReactNode {
  if (!leftOut.length) return value;

  const noReading = leftOut.filter((part) => part.reason === "noReading");
  const unsupported = leftOut.filter((part) => part.reason === "unsupported");
  const lines = [
    ...(noReading.length
      ? [`No current reading: ${noReading.map((part) => part.name).join(", ")}`]
      : []),
    ...(unsupported.length
      ? [`Not supported: ${unsupported.map((part) => part.name).join(", ")}`]
      : []),
  ];

  return (
    <Tooltip>
      <TooltipTrigger
        aria-label={`${value}; ${lines.join("; ")}`}
        className={cn(
          "rounded-sm bg-transparent p-0 whitespace-nowrap tabular-nums outline-none focus-visible:ring-2 focus-visible:ring-ring/50",
          partial && "opacity-60",
        )}
        render={<button type="button" />}
      >
        {value}
      </TooltipTrigger>
      <TooltipContent className="flex-col items-start gap-0.5 text-left">
        {lines.map((line) => (
          <span key={line}>{line}</span>
        ))}
      </TooltipContent>
    </Tooltip>
  );
}

function gpuValue(gpu: GpuReading): string {
  if (gpu.value === undefined) return "—";

  const utilization = formatChartValue("ratio", gpu.value);
  return gpu.powerWatts === undefined
    ? utilization
    : `${utilization} · ${formatChartValue("watts", gpu.powerWatts)}`;
}

const memoryKinds = ["ram", "vram"] as const;

/** A row's chart once every grid it needs is read. */
function chartOf(
  grids: readonly MetricRowChart["grid"][],
  chart: (grids: readonly MetricRowChart["grid"][]) => MetricRowChart,
): MetricRowChart | undefined {
  return grids.length ? chart(grids) : undefined;
}
