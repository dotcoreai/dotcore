import type { MetricsClient } from "@/api/metrics-client";
import {
  MetricBar,
  MetricRow,
  MetricRows,
  PendingMetricRow,
} from "@/components/metrics/metric-row/metric-row";
import {
  type ChartColor,
  formatChartValue,
} from "@/components/metrics/time-series-chart/chart-data";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { MemoryMetricRows } from "@/features/memory/memory-rows";
import { pastTotalCapacity } from "@/features/memory/pool-spans";
import { type MachineMemory, rowsFromSpans } from "@/features/memory/site-memory";
import { currentValue, fillFraction, peakValue } from "@/features/metrics/metric-current";
import type { MetricGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { type MetricRowChart, MetricRowHistory } from "@/features/metrics/metric-row-history";
import {
  type MetricPanel,
  type MetricQueryGroup,
  useMetricPanel,
} from "@/features/metrics/use-metric-panel";
import type { MetricHistoryQuery } from "@/features/metrics/use-metric-watch";
import {
  type AcceleratorFacts,
  AcceleratorMemoryArchitecture,
  AcceleratorSignal,
  AcceleratorVendor,
  type FilesystemFacts,
  type HardwareSnapshot,
} from "@/gen/dotcore/observation/v1/observation_pb";
import { formatBytes, formatCount } from "@/lib/format";

import {
  deviceMemorySeries,
  deviceSignalSeries,
  filesystemSeries,
  hostCpuSeries,
  hostMemorySeries,
} from "./machine-history-series";
import { UnavailableBadge } from "./machine-status";

/**
 * The machine's live resources as metric rows, each expanding to its history: `Resources` (RAM,
 * CPU), `Accelerators` (one group per device with its memory and signals) and `Storage` (one row
 * per real filesystem). Memory comes from the node's memory as the capacity component draws it;
 * device facts come from the hardware snapshot; CPU, accelerator signals and filesystem use come
 * from metric history, since the snapshot does not carry them. Mount it per machine, so rows start
 * collapsed for each one.
 */
export function MachineMetricSections({
  client,
  initialRange,
  machineId,
  memory,
  snapshot,
}: {
  client: MetricsClient;
  /** The range label the history charts start at, such as the range of the hosting page. */
  initialRange?: string;
  machineId: string;
  memory: MachineMemory;
  snapshot: HardwareSnapshot;
}) {
  const host = snapshot.host?.result;
  const filesystems = host?.case === "facts" ? host.value.filesystems : [];
  const devices = snapshot.accelerators
    .flatMap((observation) =>
      observation.result?.case === "facts" ? observation.result.value.facts : [],
    )
    .map((fact, index) => ({ fact, label: `GPU ${index}` }));
  const panel = useMetricPanel(
    client,
    queryGroups(machineId, devices, filesystems, memory),
    { durationMs: currentWindowMs, stepMs: currentStepMs },
    { initialRange },
  );

  return (
    <>
      <ResourcesSection memory={memory} panel={panel} />
      <AcceleratorsSection
        devices={devices}
        failedVendors={snapshot.accelerators
          .filter((observation) => observation.result?.case !== "facts")
          .map((observation) => observation.vendor)}
        memory={memory}
        panel={panel}
      />
      {filesystems.length ? <StorageSection filesystems={filesystems} panel={panel} /> : null}
    </>
  );
}

/**
 * The machine's `Resources` section before its hardware snapshot is read: RAM and CPU, the rows
 * every machine has, empty, so they hold their place until `MachineMetricSections` replaces them.
 */
export function PendingMachineResources() {
  return (
    <section aria-labelledby="machine-resources-heading">
      <InspectorSectionHeader headingId="machine-resources-heading" title="Resources" />
      <MetricRows className="mt-3">
        <PendingMetricRow label="RAM" />
        <PendingMetricRow label="CPU" />
      </MetricRows>
    </section>
  );
}

// Machine readings arrive every second, so current values follow them closely, open buckets
// included since a gauge's open bucket is a real reading, and a reading that stops for a few
// seconds no longer counts as current. The window gives power its peak.
const currentWindowMs = 15 * 60_000;
const currentStepMs = 1000;
const freshSteps = 5;
const fresh = { includeOpen: true } as const;

/** A GPU's temperature bar fills against this fixed reference until devices report their limits. */
const temperatureReferenceCelsius = 100;

/** One accelerator in display order, named by its position across vendors. */
interface MachineDevice {
  fact: AcceleratorFacts;
  label: string;
}

/** The accelerator signals in display order, with what their row reads and how its bar fills. */
const signalRows: {
  color: ChartColor;
  group: string;
  label: string;
  metric: string;
  /** The value the bar fills against, given the device's current window. */
  reference: (current: MetricGrid | undefined, deviceId: string) => number | undefined;
  signal: AcceleratorSignal;
  unit: "celsius" | "ratio" | "watts";
}[] = [
  {
    color: "chart-1",
    group: "accelerator.utilization",
    label: "Utilization",
    metric: metricNames.nodeAcceleratorUtilization,
    reference: () => 1,
    signal: AcceleratorSignal.UTILIZATION,
    unit: "ratio",
  },
  {
    color: "chart-4",
    group: "accelerator.temperature",
    label: "Temperature",
    metric: metricNames.nodeAcceleratorTemperature,
    reference: () => temperatureReferenceCelsius,
    signal: AcceleratorSignal.TEMPERATURE,
    unit: "celsius",
  },
  {
    color: "chart-3",
    group: "accelerator.power",
    label: "Power",
    metric: metricNames.nodeAcceleratorPower,
    reference: (current, deviceId) => peakValue(deviceSeries(current, deviceId)),
    signal: AcceleratorSignal.POWER,
    unit: "watts",
  },
];

/** RAM from the snapshot, then CPU utilization. */
function ResourcesSection({ memory, panel }: { memory: MachineMemory; panel: MetricPanel }) {
  const ramRows = rowsFromSpans(memory.spans).filter((row) => row.kind === "ram");
  const [cpu] = panel.current("cpu") ?? [];
  const cpuValue = currentValue(cpu?.series[0], freshSteps, fresh);
  const [cpuHistory] = panel.history("cpu") ?? [];
  const [memoryTotal, memoryAvailable] = panel.history("memory") ?? [];

  return (
    <section aria-labelledby="machine-resources-heading">
      <InspectorSectionHeader headingId="machine-resources-heading" title="Resources" />
      <MetricRows className="mt-3">
        {ramRows.length ? (
          <MemoryMetricRows
            capacity={pastTotalCapacity(ramRows)}
            expanded={() => panel.expanded("ram")}
            onExpandedChange={(_, expanded) => panel.setExpanded("ram", expanded)}
            rows={ramRows}
            unavailable={memory.unavailable}
          >
            {() => (
              <MetricRowHistory
                chart={
                  memoryTotal && memoryAvailable
                    ? { grid: memoryTotal, series: hostMemorySeries(memoryTotal, memoryAvailable) }
                    : undefined
                }
                label="RAM history"
                panel={panel}
                unit="bytes"
              />
            )}
          </MemoryMetricRows>
        ) : null}
        <MetricRow
          bar={<MetricBar fraction={cpuValue} />}
          expanded={panel.expanded("cpu")}
          label="CPU"
          onExpandedChange={(expanded) => panel.setExpanded("cpu", expanded)}
          value={readout("ratio", cpuValue)}
        >
          <MetricRowHistory
            chart={cpuHistory ? { grid: cpuHistory, series: hostCpuSeries(cpuHistory) } : undefined}
            label="CPU history"
            panel={panel}
            unit="ratio"
          />
        </MetricRow>
      </MetricRows>
      {ramRows.length ? null : (
        <DetailList>
          <Detail label="Memory" value={<UnavailableBadge />} />
        </DetailList>
      )}
      <CurrentFailedStatus panel={panel} />
    </section>
  );
}

/**
 * All accelerators of the machine as one section whose meta value is the total device count. Each
 * device is a group: its identifying facts, then its memory row for a dedicated GPU and a row per
 * supported signal. A unified GPU has no memory row, since its memory is the machine's RAM.
 * Vendors are observed and fail independently, so a vendor that failed adds its own `Unavailable`
 * row while the other vendors' devices still render.
 */
function AcceleratorsSection({
  devices,
  failedVendors,
  memory,
  panel,
}: {
  devices: readonly MachineDevice[];
  failedVendors: readonly AcceleratorVendor[];
  memory: MachineMemory;
  panel: MetricPanel;
}) {
  return (
    <section aria-labelledby="machine-accelerators-heading">
      <InspectorSectionHeader
        headingId="machine-accelerators-heading"
        meta={formatCount(devices.length, "device")}
        title="Accelerators"
      />
      {devices.map((device, index) => (
        <AcceleratorGroup
          device={device}
          first={index === 0}
          key={device.fact.deviceId || `observed-device-${index}`}
          memory={memory}
          panel={panel}
        />
      ))}
      {failedVendors.length ? (
        <div className={devices.length ? "mt-3 border-t border-border pt-3" : ""}>
          <DetailList>
            {failedVendors.map((vendor) => (
              <Detail key={vendor} label={vendorLabel(vendor)} value={<UnavailableBadge />} />
            ))}
          </DetailList>
        </div>
      ) : null}
      {!devices.length && !failedVendors.length ? (
        <DetailList>
          <Detail label="Devices" value="None detected" />
        </DetailList>
      ) : null}
    </section>
  );
}

/**
 * One device: its name, the facts its vendor reports, then its metric rows. Facts the vendor does
 * not report, such as a driver version for an Apple GPU, have no row.
 */
function AcceleratorGroup({
  device,
  first,
  memory,
  panel,
}: {
  device: MachineDevice;
  first: boolean;
  memory: MachineMemory;
  panel: MetricPanel;
}) {
  const { fact, label } = device;
  const unified = fact.memoryArchitecture === AcceleratorMemoryArchitecture.UNIFIED;
  const span = memory.deviceSpans.get(fact.deviceId);
  // The device reports no memory, or the machine's memory can't be read at all.
  const unmeasured =
    fact.totalMemoryBytes === undefined ||
    fact.usedMemoryBytes === undefined ||
    (!span && memory.unavailable.some((entry) => entry.kind === undefined));
  const spanRows = span ? rowsFromSpans([span]) : [];
  const signals = signalRows.filter((row) => fact.supportedSignals.includes(row.signal));
  const [vramHistory] = panel.history("accelerator.memory") ?? [];
  const rowId = (kind: string) => `${kind}:${fact.deviceId}`;

  return (
    <div
      aria-label={`${label} · ${fact.model}`}
      className={first ? "mt-3" : "mt-3 border-t border-border pt-3"}
      role="group"
    >
      <p className="text-xs font-medium">
        {label} · {fact.model}
      </p>
      {fact.driverOrRuntimeVersion ||
      unified ||
      fact.workingSetLimitBytes !== undefined ||
      unmeasured ? (
        <DetailList>
          {fact.driverOrRuntimeVersion ? (
            <Detail label="Runtime version" value={fact.driverOrRuntimeVersion} />
          ) : null}
          {unified ? <Detail label="Memory" value="Unified" /> : null}
          {fact.workingSetLimitBytes === undefined ? null : (
            <Detail label="GPU working-set limit" value={formatBytes(fact.workingSetLimitBytes)} />
          )}
          {!unified && unmeasured ? <Detail label="VRAM" value={<UnavailableBadge />} /> : null}
        </DetailList>
      ) : null}
      {span || signals.length ? (
        <MetricRows className="mt-3">
          {span ? (
            <MemoryMetricRows
              capacity={pastTotalCapacity(spanRows)}
              deviceLabel={`${fact.model} · ${label}`}
              expanded={() => panel.expanded(rowId("vram"))}
              onExpandedChange={(_, expanded) => panel.setExpanded(rowId("vram"), expanded)}
              rows={spanRows}
            >
              {() => (
                <MetricRowHistory
                  chart={
                    vramHistory
                      ? {
                          grid: vramHistory,
                          series: deviceMemorySeries(
                            fact.deviceId,
                            vramHistory,
                            Number(span.totalBytes),
                          ),
                        }
                      : undefined
                  }
                  label={`${label} VRAM history`}
                  panel={panel}
                  unit="bytes"
                />
              )}
            </MemoryMetricRows>
          ) : null}
          {signals.map((signal) => {
            const [current] = panel.current(signal.group) ?? [];
            const value = currentValue(deviceSeries(current, fact.deviceId), freshSteps, fresh);
            const [history] = panel.history(signal.group) ?? [];
            const chart: MetricRowChart | undefined = history
              ? {
                  grid: history,
                  series: [deviceSignalSeries(fact.deviceId, history, signal.label, signal.color)],
                }
              : undefined;

            return (
              <MetricRow
                bar={
                  <MetricBar
                    color={signal.color}
                    fraction={fillFraction(value, signal.reference(current, fact.deviceId))}
                  />
                }
                expanded={panel.expanded(rowId(signal.group))}
                key={signal.group}
                label={signal.label}
                onExpandedChange={(expanded) => panel.setExpanded(rowId(signal.group), expanded)}
                value={readout(signal.unit, value)}
              >
                <MetricRowHistory
                  chart={chart}
                  label={`${label} ${signal.label.toLowerCase()} history`}
                  panel={panel}
                  unit={signal.unit}
                />
              </MetricRow>
            );
          })}
        </MetricRows>
      ) : null}
    </div>
  );
}

/** One row per real filesystem: used space from metric history against the snapshot's size. */
function StorageSection({
  filesystems,
  panel,
}: {
  filesystems: readonly FilesystemFacts[];
  panel: MetricPanel;
}) {
  const [current] = panel.current("filesystem") ?? [];
  const [used, size] = panel.history("filesystem") ?? [];

  return (
    <section aria-labelledby="machine-storage-heading">
      <InspectorSectionHeader
        headingId="machine-storage-heading"
        meta={formatCount(filesystems.length, "filesystem")}
        title="Storage"
      />
      <MetricRows className="mt-3">
        {filesystems.map((filesystem) => {
          const rowId = `filesystem:${filesystem.mountpoint}`;
          const value = currentValue(
            current?.series.find(
              (series) =>
                series.labels[dimensionNames.mountpoint] === filesystem.mountpoint &&
                series.labels[dimensionNames.fstype] === filesystem.fstype,
            ),
            freshSteps,
            fresh,
          );
          const sizeBytes = Number(filesystem.sizeBytes);

          return (
            <MetricRow
              bar={<MetricBar color="chart-2" fraction={fillFraction(value, sizeBytes)} />}
              expanded={panel.expanded(rowId)}
              key={filesystem.mountpoint}
              label={filesystem.mountpoint}
              onExpandedChange={(expanded) => panel.setExpanded(rowId, expanded)}
              value={`${value === undefined ? "—" : formatBytes(BigInt(Math.round(value)))} / ${formatBytes(filesystem.sizeBytes)}`}
            >
              <MetricRowHistory
                chart={
                  used && size
                    ? { grid: used, series: filesystemSeries(filesystem, used, size) }
                    : undefined
                }
                label={`${filesystem.mountpoint} filesystem history`}
                panel={panel}
                unit="bytes"
              />
            </MetricRow>
          );
        })}
      </MetricRows>
    </section>
  );
}

/** States that current readings stopped updating; the last ones read stay in view. */
export function CurrentFailedStatus({ panel }: { panel: MetricPanel }) {
  return panel.currentFailed ? (
    <p className="mt-2 text-xs text-muted-foreground" role="status">
      Current readings stopped updating; showing the last values read.
    </p>
  ) : null;
}

/**
 * The query groups the machine's rows read: one per kind of reading, grouped by device or
 * filesystem so every device's row shares it. Groups for readings the machine cannot have, such as
 * a signal no device supports, are left out.
 */
function queryGroups(
  machineId: string,
  devices: readonly MachineDevice[],
  filesystems: readonly FilesystemFacts[],
  memory: MachineMemory,
): MetricQueryGroup[] {
  const query = (metric: string, ...groupBy: string[]): MetricHistoryQuery => ({
    groupBy,
    matchers: { [dimensionNames.node]: machineId },
    metric,
  });
  const groups: MetricQueryGroup[] = [
    {
      current: [],
      history: [
        query(metricNames.nodeHostMemoryTotal, dimensionNames.node),
        query(metricNames.nodeHostMemoryAvailable, dimensionNames.node),
      ],
      id: "memory",
    },
    {
      current: [query(metricNames.nodeHostCpuUtilization, dimensionNames.node)],
      history: [query(metricNames.nodeHostCpuUtilization, dimensionNames.node)],
      id: "cpu",
    },
  ];

  if (memory.deviceSpans.size > 0) {
    groups.push({
      current: [],
      history: [query(metricNames.nodeAcceleratorMemoryUsed, dimensionNames.device)],
      id: "accelerator.memory",
    });
  }
  for (const signal of signalRows) {
    if (devices.some((device) => device.fact.supportedSignals.includes(signal.signal))) {
      const signalQuery = query(signal.metric, dimensionNames.device);
      groups.push({ current: [signalQuery], history: [signalQuery], id: signal.group });
    }
  }
  if (filesystems.length > 0) {
    const used = query(
      metricNames.nodeFilesystemUsed,
      dimensionNames.mountpoint,
      dimensionNames.fstype,
    );
    groups.push({
      current: [used],
      history: [
        used,
        query(metricNames.nodeFilesystemSize, dimensionNames.mountpoint, dimensionNames.fstype),
      ],
      id: "filesystem",
    });
  }

  return groups;
}

function deviceSeries(grid: MetricGrid | undefined, deviceId: string) {
  return grid?.series.find((series) => series.labels[dimensionNames.device] === deviceId);
}

/** A current reading as text, or a dash when there is none. */
function readout(unit: "celsius" | "ratio" | "watts", value: number | undefined): string {
  return value === undefined ? "—" : formatChartValue(unit, value);
}

function vendorLabel(vendor: AcceleratorVendor): string {
  switch (vendor) {
    case AcceleratorVendor.NVIDIA:
      return "NVIDIA";
    case AcceleratorVendor.AMD:
      return "AMD";
    case AcceleratorVendor.APPLE:
      return "Apple";
    default:
      return "Accelerator";
  }
}
