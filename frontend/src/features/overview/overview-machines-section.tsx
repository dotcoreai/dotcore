import type { RefObject } from "react";

import type { MetricsClient } from "@/api/metrics-client";
import { MetricCard, MetricCardGrid } from "@/components/metrics/metric-card/metric-card";
import { Sparkline } from "@/components/metrics/sparkline/sparkline";
import { Notice } from "@/components/ui/notice";
import { PageSectionHeading } from "@/components/ui/page-header";
import { nodeConnectionLabel } from "@/features/deployments/execution-target";
import { machineEntry } from "@/features/machines/machine-panels";
import { machineName } from "@/features/memory/described-machines";
import { CardChartDetail, ReadingsStoppedNote } from "@/features/metrics/card-panel";
import type { MetricCard as MetricCardModel } from "@/features/metrics/use-metric-cards";
import { NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";

import type { MachineNow } from "./machine-site-values";
import {
  formatReading,
  formatUsed,
  machineCardHeadline,
  machineCardIds,
  type MachineHeadlineContext,
  machineSplitId,
} from "./overview-machine-specs";
import { OverviewTable, type OverviewTableColumn, type OverviewTableRow } from "./overview-table";
import { type MachinesNow, useMachinesNow } from "./use-machines-now";
import type { OverviewMetrics } from "./use-overview-metrics";

const splitOptions = [{ label: "By machine", value: machineSplitId }];

/**
 * The overview's machines section: the connected count and the site's hardware cards with
 * sparklines, the selected card's chart which can be broken down by machine, and a table of the
 * machines' current readings. A card's headline is the newest closed reading from the "now" stream;
 * its line and sparkline cover the page range. A table row opens the machine's sidebar.
 */
export function OverviewMachinesSection({
  metrics,
  metricsClient,
  openerRef,
}: {
  metrics: OverviewMetrics;
  metricsClient: MetricsClient;
  /** Receives the row that opened the sidebar, so focus returns to it when the sidebar closes. */
  openerRef: RefObject<HTMLElement | null>;
}) {
  const { facts, factsPending, machines } = metrics.fleet;
  const now = useMachinesNow(metricsClient, facts);
  const cards = machineCardIds.flatMap((id) => metrics.cards.find((card) => card.id === id) ?? []);
  const selected =
    metrics.selected && isMachineCard(metrics.selected) ? metrics.selected : undefined;
  const chart = selected && metrics.chart?.id === selected ? metrics.chart : undefined;
  const connected = machines?.filter(
    (machine) => machine.observationFreshness?.state === NodeObservationState.CONNECTED,
  ).length;
  const context: MachineHeadlineContext = {
    facts,
    factsPending,
    machineCount: machines?.length ?? 0,
    now: now?.site,
  };

  return (
    <section aria-labelledby="overview-machines-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="overview-machines-heading">Machines</PageSectionHeading>
      {metrics.unavailable ? (
        <Notice tone="destructive">Machine metrics could not be read.</Notice>
      ) : machines?.length === 0 ? (
        <p className="text-sm text-muted-foreground">No machines configured.</p>
      ) : (
        <>
          <MetricCardGrid
            className="lg:grid-cols-3 xl:grid-cols-4"
            columns={2}
            onSelectedChange={metrics.setSelected}
            selected={selected}
          >
            <MetricCard
              id="machines"
              label="Machines"
              line={machines ? `of ${machines.length} listed` : undefined}
              pending={connected === undefined}
              selectable={false}
              unit="connected"
              value={String(connected ?? 0)}
            />
            {cards.map((card) => (
              <MachineCard card={card} context={context} key={card.id} now={now} />
            ))}
          </MetricCardGrid>
          {chart ? (
            <CardChartDetail
              chart={chart}
              height={160}
              split={{
                label: "Split machine chart by",
                onChange: metrics.setSplit,
                options: splitOptions,
                value: metrics.split,
              }}
            />
          ) : null}
          <MachinesTable metrics={metrics} now={now} openerRef={openerRef} />
        </>
      )}
      <ReadingsStoppedNote failed={metrics.failed} subject="Machine" />
    </section>
  );
}

function MachineCard({
  card,
  context,
  now,
}: {
  card: MetricCardModel;
  context: MachineHeadlineContext;
  now: MachinesNow | undefined;
}) {
  const headline = machineCardHeadline(card.id, card.reading, context);

  return (
    <MetricCard
      id={card.id}
      label={card.label}
      line={headline.line}
      pending={card.reading === undefined && now === undefined}
      sparkline={<Sparkline values={card.reading?.sparkline ?? []} variant="line" />}
      unit={headline.unit}
      value={headline.value}
    />
  );
}

/** The machines table's columns, each with how a cell shows a machine's newest reading. */
const columns: (OverviewTableColumn & { format: (now: MachineNow) => string })[] = [
  { format: (now) => formatReading("ratio", now.cpu), key: "cpu", label: "CPU" },
  {
    format: (now) => formatReading("ratio", now.gpu),
    key: "gpu",
    label: "GPU",
    minWidth: "@sm:table-cell",
  },
  {
    format: (now) => formatUsed(now.ramUsed, now.ramTotal),
    key: "ram",
    label: "RAM",
    minWidth: "@md:table-cell",
  },
  {
    format: (now) => formatUsed(now.vramUsed, now.vramTotal),
    key: "vram",
    label: "VRAM",
    minWidth: "@xl:table-cell",
  },
  {
    format: (now) => formatUsed(now.storageUsed, now.storageSize),
    key: "storage",
    label: "Storage",
    minWidth: "@2xl:table-cell",
  },
];

function MachinesTable({
  metrics,
  now,
  openerRef,
}: {
  metrics: OverviewMetrics;
  now: MachinesNow | undefined;
  openerRef: RefObject<HTMLElement | null>;
}) {
  const { machines, summaries } = metrics.fleet;
  const rows = (machines ?? []).map((machine): OverviewTableRow => {
    const connected = machine.observationFreshness?.state === NodeObservationState.CONNECTED;
    const reading = connected ? now?.machines.get(machine.machineId) : undefined;

    return {
      cells: columns.map((column) => (reading ? column.format(reading) : "—")),
      colorKey: machine.machineId,
      entry: machineEntry(machine.machineId),
      id: machine.machineId,
      name: machineName(machine),
      subtitle: connected
        ? (summaries.get(machine.machineId) ?? "")
        : nodeConnectionLabel(machine.observationFreshness?.state),
    };
  });

  return <OverviewTable columns={columns} label="Machines" openerRef={openerRef} rows={rows} />;
}

function isMachineCard(id: string): boolean {
  return (machineCardIds as readonly string[]).includes(id);
}
