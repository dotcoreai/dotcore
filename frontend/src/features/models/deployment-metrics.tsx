import { createContext, type ReactNode, use, useMemo } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { MetricRows } from "@/components/metrics/metric-row/metric-row";
import {
  capacitySeries,
  type ChartSeries,
} from "@/components/metrics/time-series-chart/chart-data";
import { Detail, DetailList } from "@/components/ui/detail-list";
import { StatusBadge } from "@/components/ui/status-badge";
import { MemoryMetricRows } from "@/features/memory/memory-rows";
import { pastTotalCapacity } from "@/features/memory/pool-spans";
import { machineMemory, type MemoryRow, type SiteMemory } from "@/features/memory/site-memory";
import type { SiteMemoryState } from "@/features/memory/use-site-memory";
import type { MetricGrid } from "@/features/metrics/metric-grid";
import { dimensionNames, metricNames } from "@/features/metrics/metric-names";
import { MetricRowHistory } from "@/features/metrics/metric-row-history";
import { deploymentRequestCards } from "@/features/metrics/request-card-specs";
import { RequestCardsSection } from "@/features/metrics/request-cards";
import { type MetricCards, useMetricCards } from "@/features/metrics/use-metric-cards";
import { type MetricPanel, useMetricPanel } from "@/features/metrics/use-metric-panel";
import type { ModelDeployment } from "@/gen/dotcore/model/v1/deployment_pb";

/**
 * The metric state of one model deployment's inspector, shared by its memory row and its
 * `Requests` cards so both follow the cards' range and the inspector holds at most two streams: the
 * cards, and the memory history while its row is expanded. Mount it per deployment, so rows start
 * collapsed and the cards start afresh for each one.
 */
export function DeploymentMetricsProvider({
  active,
  children,
  client,
  deployment,
  initialRange,
}: {
  /**
   * Whether the inspector shows its metrics; while it doesn't, no metric stream is open and the
   * last values read stay for when it shows them again.
   */
  active: boolean;
  children: ReactNode;
  client: MetricsClient;
  deployment: ModelDeployment;
  /** The range label the cards start at, such as the range of the page the sidebar opened from. */
  initialRange?: string;
}) {
  const deploymentId = deployment.modelDeploymentId;
  const specs = useMemo(() => deploymentRequestCards(deploymentId), [deploymentId]);
  const cards = useMetricCards(client, specs, { initialRange, paused: !active });
  const panel = useMetricPanel(
    client,
    [{ current: [], history: [memoryQuery(deploymentId)], id: memoryGroup }],
    noCurrentWindow,
    { paused: !active, rangeLabel: cards.range?.label },
  );
  return <DeploymentMetricsContext value={{ cards, panel }}>{children}</DeploymentMetricsContext>;
}

/**
 * The deployment's memory rows in its `Deployment` section: the rows of the machine it runs on,
 * with its own segment colored, the other models grayed, and readouts of its own usage. The row
 * holding its accelerator memory (VRAM on a machine with dedicated GPUs, RAM on unified memory)
 * expands to the deployment's attributed memory over time, summed across its devices, against
 * the memory of its own GPUs (or the machine's RAM); another row doesn't expand. While memory is
 * first read, an empty RAM row holds the rows' place; a status replaces the rows while the
 * deployment has no machine or its machine's memory can't be read.
 */
export function DeploymentMemoryRows({
  deployment,
  state,
}: {
  deployment: ModelDeployment;
  state: SiteMemoryState;
}) {
  const { panel } = useDeploymentMetrics();
  const machineId = deploymentMachineId(deployment);
  const memory =
    machineId && state.kind !== "loading" && state.memory
      ? machineMemory(state.memory, machineId)
      : undefined;
  const [history] = panel.history(memoryGroup) ?? [];

  // Placement is known before memory, so an unplaced deployment never shows a row it then drops.
  if (!machineId) return <MemoryStatus label="Not placed" />;
  if (state.kind === "loading") {
    return (
      <MetricRows className="mt-3">
        <MemoryMetricRows
          expanded={() => false}
          onExpandedChange={() => {}}
          pending={["ram"]}
          rows={[]}
        >
          {() => null}
        </MemoryMetricRows>
      </MetricRows>
    );
  }
  if (!memory?.rows.length) return <MemoryStatus label="Unavailable" />;

  const acceleratorKind = memory.rows.some((row) => row.kind === "vram") ? "vram" : "ram";
  const capacity = (row: MemoryRow) =>
    row.kind === "ram"
      ? Number(row.totalBytes)
      : deviceTotalBytes(deployment.modelDeploymentId, state.memory, machineId);

  return (
    <MetricRows className="mt-3">
      <MemoryMetricRows
        capacity={pastTotalCapacity(memory.rows)}
        expanded={(kind) => panel.expanded(`memory:${kind}`)}
        focusDeploymentId={deployment.modelDeploymentId}
        onExpandedChange={(kind, expanded) => panel.setExpanded(`memory:${kind}`, expanded)}
        rows={memory.rows}
        unavailable={memory.unavailable}
      >
        {(row) =>
          row.kind === acceleratorKind ? (
            <MetricRowHistory
              chart={
                history
                  ? { grid: history, series: memorySeries(history, capacity(row)) }
                  : undefined
              }
              label="Deployment memory history"
              panel={panel}
              rangePicker={false}
              unit="bytes"
            />
          ) : null
        }
      </MemoryMetricRows>
    </MetricRows>
  );
}

/** The deployment's `Requests` section, measured per attempt. */
export function DeploymentRequests() {
  const { cards } = useDeploymentMetrics();

  return <RequestCardsSection headingId="deployment-requests-heading" metrics={cards} />;
}

const DeploymentMetricsContext = createContext<
  { cards: MetricCards; panel: MetricPanel } | undefined
>(undefined);

const memoryGroup = "memory";

// The memory group reads history only.
const noCurrentWindow = { durationMs: 0, stepMs: 0 } as const;

function useDeploymentMetrics() {
  const context = use(DeploymentMetricsContext);
  if (!context) throw new Error("deployment metrics used outside their provider");

  return context;
}

function MemoryStatus({ label }: { label: "Loading" | "Not placed" | "Unavailable" }) {
  return (
    <DetailList>
      <Detail label="Memory" value={<StatusBadge status={{ label, tone: "neutral" }} />} />
    </DetailList>
  );
}

/** The machine a deployment runs on: its observed placement, else its attributed memory rows. */
function deploymentMachineId(deployment: ModelDeployment): string | undefined {
  const placed = deployment.state?.observedPlacements[0]?.managedNodeId;
  if (placed) return placed;

  const result = deployment.acceleratorMemory?.result;
  return result?.case === "available" ? result.value.devices[0]?.managedNodeId : undefined;
}

/** The deployment's attributed memory, summed across its devices. */
function memoryQuery(deploymentId: string) {
  return {
    groupBy: [],
    matchers: { [dimensionNames.deployment]: deploymentId },
    metric: metricNames.deploymentAcceleratorMemoryUsed,
  };
}

/**
 * Used memory per step against its capacity when known; absent steps where no attribution was
 * recorded.
 */
function memorySeries(grid: MetricGrid, totalBytes: number | undefined): ChartSeries[] {
  const series = grid.series[0];

  return [
    {
      color: "chart-1",
      id: "used",
      label: "Used",
      provisional: series?.provisional,
      values: series?.values ?? Array.from({ length: grid.stepCount }, () => null),
    },
    ...(totalBytes ? [capacitySeries(totalBytes, grid.stepCount)] : []),
  ];
}

/**
 * The dedicated memory of the GPUs holding the deployment's memory on its machine; unknown when
 * none of them holds any.
 */
function deviceTotalBytes(
  deploymentId: string,
  memory: SiteMemory | undefined,
  machineId: string,
): number | undefined {
  const total = [...(memory?.machines.get(machineId)?.deviceSpans.values() ?? [])]
    .filter((span) => span.segments.some((segment) => segment.deploymentId === deploymentId))
    .reduce((sum, span) => sum + Number(span.totalBytes), 0);

  return total || undefined;
}
