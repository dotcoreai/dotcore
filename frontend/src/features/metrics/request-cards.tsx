import type { MetricsClient } from "@/api/metrics-client";
import { MetricCardGrid } from "@/components/metrics/metric-card/metric-card";
import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Notice } from "@/components/ui/notice";
import { SegmentedControl } from "@/components/ui/segmented-control";

import { CardChartDetail, ReadingCard, ReadingsStoppedNote } from "./card-panel";
import { type MetricCards, type MetricCardSpec, useMetricCards } from "./use-metric-cards";

/**
 * The `Requests` inspector section of a subject that has no other metric in its sidebar: it reads
 * its cards itself and shows them. Mount it per subject, so the range and the selected card start
 * afresh for each one.
 */
export function RequestCards({
  client,
  headingId,
  initialRange,
  paused,
  specs,
}: {
  client: MetricsClient;
  headingId: string;
  /** The range label the cards start at, such as the range of the page the sidebar opened from. */
  initialRange?: string;
  /** Pauses the cards, such as for a sidebar hidden behind another view. */
  paused?: boolean;
  specs: readonly MetricCardSpec[];
}) {
  const metrics = useMetricCards(client, specs, { initialRange, paused });

  return <RequestCardsSection headingId={headingId} metrics={metrics} />;
}

/**
 * The `Requests` section for cards read by the caller: the range picker in the header, the cards in
 * three columns without sparklines, and the selected card's chart below them. The cards show from
 * the start, empty until values are read, so the section keeps its size.
 */
export function RequestCardsSection({
  headingId,
  metrics,
}: {
  headingId: string;
  metrics: MetricCards;
}) {
  const { chart, ranges } = metrics;

  return (
    <section aria-labelledby={headingId}>
      <InspectorSectionHeader
        action={
          ranges.length > 1 ? (
            <SegmentedControl
              aria-label="Requests range"
              onValueChange={metrics.setRange}
              options={ranges.map((range) => ({ label: range.label, value: range.label }))}
              value={metrics.range?.label}
            />
          ) : undefined
        }
        headingId={headingId}
        title="Requests"
      />
      {metrics.unavailable ? (
        <Notice className="mt-3" tone="destructive">
          Requests could not be read.
        </Notice>
      ) : (
        <div className="mt-3 flex flex-col gap-3">
          <MetricCardGrid onSelectedChange={metrics.setSelected} selected={metrics.selected}>
            {metrics.cards.map((card) => (
              <ReadingCard card={card} key={card.id} />
            ))}
          </MetricCardGrid>
          {chart ? <CardChartDetail chart={chart} height={120} /> : null}
        </div>
      )}
      <ReadingsStoppedNote className="mt-2" failed={metrics.failed} subject="Request" />
    </section>
  );
}
