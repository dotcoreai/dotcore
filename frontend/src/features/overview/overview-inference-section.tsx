import { MetricCardGrid } from "@/components/metrics/metric-card/metric-card";
import { Notice } from "@/components/ui/notice";
import { PageSectionHeading } from "@/components/ui/page-header";
import { CardChartDetail, ReadingCard, ReadingsStoppedNote } from "@/features/metrics/card-panel";
import type { MetricCards } from "@/features/metrics/use-metric-cards";

import { inferenceCardIds, overviewSplitIds } from "./overview-card-specs";

const splitOptions = [
  { label: "By model", value: overviewSplitIds.model },
  { label: "By route", value: overviewSplitIds.route },
];

/**
 * The overview's inference section: the site's request cards with sparklines in columns, and
 * below them the selected card's chart, which can be broken down by model or by route. It shows
 * only the inference cards of the page's shared metrics, so a card of another section never draws
 * its chart here.
 */
export function OverviewInferenceSection({ metrics }: { metrics: MetricCards }) {
  const cards = metrics.cards.filter((card) => isInferenceCard(card.id));
  const selected =
    metrics.selected && isInferenceCard(metrics.selected) ? metrics.selected : undefined;
  const chart = selected && metrics.chart?.id === selected ? metrics.chart : undefined;

  return (
    <section aria-labelledby="overview-inference-heading" className="flex flex-col gap-3">
      <PageSectionHeading id="overview-inference-heading">Inference</PageSectionHeading>
      {metrics.unavailable ? (
        <Notice tone="destructive">Inference metrics could not be read.</Notice>
      ) : (
        <>
          <MetricCardGrid
            className="lg:grid-cols-3 xl:grid-cols-4"
            columns={2}
            onSelectedChange={metrics.setSelected}
            selected={selected}
          >
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
