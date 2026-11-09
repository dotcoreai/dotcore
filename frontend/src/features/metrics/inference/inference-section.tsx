import type { ReactNode } from "react";

import { MetricCardGrid } from "@/components/metrics/metric-card/metric-card";
import { Notice } from "@/components/ui/notice";
import { PageSectionHeading } from "@/components/ui/page-header";
import { CardChartDetail, ReadingCard, ReadingsStoppedNote } from "@/features/metrics/card-panel";
import type { MetricCards } from "@/features/metrics/use-metric-cards";

import { inferenceCardIds, inferenceSplitIds } from "./inference-card-specs";

const splitOptions = [
  { label: "By model", value: inferenceSplitIds.model },
  { label: "By route", value: inferenceSplitIds.route },
];

/**
 * A page's inference section: the request cards with sparklines in columns, and below them the
 * selected card's chart, which can be broken down by model or by route. It shows only the
 * inference cards of the page's metrics, so a card of another section never draws its chart here.
 * Leading cards come first and stay when the metrics cannot be read.
 */
export function InferenceSection({
  leading,
  metrics,
}: {
  /** Cards shown before the request cards, such as counts; they do not depend on the metrics. */
  leading?: ReactNode;
  metrics: MetricCards;
}) {
  const cards = metrics.cards.filter((card) => isInferenceCard(card.id));
  const selected =
    metrics.selected && isInferenceCard(metrics.selected) ? metrics.selected : undefined;
  const chart = selected && metrics.chart?.id === selected ? metrics.chart : undefined;

  return (
    <section aria-labelledby="inference-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="inference-heading">Inference</PageSectionHeading>
      {metrics.unavailable ? (
        <>
          {leading ? (
            <div className="grid grid-cols-2 gap-2 lg:grid-cols-3 xl:grid-cols-4">{leading}</div>
          ) : null}
          <Notice tone="destructive">Inference metrics could not be read.</Notice>
        </>
      ) : (
        <>
          <MetricCardGrid
            className="lg:grid-cols-3 xl:grid-cols-4"
            columns={2}
            onSelectedChange={metrics.setSelected}
            selected={selected}
          >
            {leading}
            {cards.map((card) => (
              <ReadingCard card={card} key={card.id} sparkline />
            ))}
          </MetricCardGrid>
          {chart ? (
            <CardChartDetail
              chart={chart}
              height={160}
              split={{
                label: "Split inference chart by",
                onChange: metrics.setSplit,
                options: splitOptions,
                value: metrics.split,
              }}
            />
          ) : null}
        </>
      )}
      <ReadingsStoppedNote failed={metrics.failed} subject="Inference" />
    </section>
  );
}

function isInferenceCard(id: string): boolean {
  return (inferenceCardIds as readonly string[]).includes(id);
}
