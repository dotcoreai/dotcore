import type { MetricsClient } from "@/api/metrics-client";
import { MetricCard, MetricCardGrid } from "@/components/metrics/metric-card/metric-card";
import { Sparkline } from "@/components/metrics/sparkline/sparkline";
import { Notice } from "@/components/ui/notice";
import { PageSectionHeading } from "@/components/ui/page-header";
import { CardChartDetail, ReadingsStoppedNote } from "@/features/metrics/card-panel";
import type { MetricCard as MetricCardModel } from "@/features/metrics/use-metric-cards";
import { NodeObservationState } from "@/gen/dotcore/site/v1/service_pb";

import { OverviewCountCard } from "./overview-count-cards";
import {
  machineCardHeadline,
  machineCardIds,
  type MachineHeadlineContext,
  machineSplitId,
} from "./overview-machine-specs";
import { type MachinesNow, useMachinesNow } from "./use-machines-now";
import type { OverviewMetrics } from "./use-overview-metrics";

const splitOptions = [{ label: "By machine", value: machineSplitId }];

/**
 * The overview's machines section: the connected count and the site's hardware cards with
 * sparklines, and the selected card's chart which can be broken down by machine. A card's headline
 * is the newest closed reading from the "now" stream; its line and sparkline cover the page range.
 * The connected count opens the machines page.
 */
export function OverviewMachinesSection({
  metrics,
  metricsClient,
}: {
  metrics: OverviewMetrics;
  metricsClient: MetricsClient;
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
            <OverviewCountCard
              label="Machines"
              line={machines ? `of ${machines.length} listed` : undefined}
              pending={connected === undefined}
              to="/machines"
              unit="connected"
              value={connected ?? 0}
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

function isMachineCard(id: string): boolean {
  return (machineCardIds as readonly string[]).includes(id);
}
