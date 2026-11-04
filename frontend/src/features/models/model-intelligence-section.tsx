import { InspectorSectionHeader } from "@/components/ui/inspector-section-header";
import { Progress, ProgressLabel, ProgressValue } from "@/components/ui/progress";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";

import { formatReferenceScore } from "./catalog-table-model";

/**
 * The Intelligence section of a catalog model inspector: the model's overall Dotcore Bench
 * Intelligence Score as a labeled bar on the 0–100 scale. A model without a score renders nothing.
 */
export function ModelIntelligenceSection({ model }: { model: Model }) {
  const score = model.intelligence;
  if (!score) return null;

  const value = formatReferenceScore(score.overall);

  return (
    <section aria-labelledby="catalog-model-intelligence-heading">
      <InspectorSectionHeader headingId="catalog-model-intelligence-heading" title="Intelligence" />
      <p className="mt-1 text-xs text-muted-foreground">Dotcore Bench Intelligence Score</p>
      <Progress
        // A score is not a completion percentage, which the primitive announces by default.
        getAriaValueText={() => `${value} out of 100`}
        className="mt-3 grid grid-cols-[6rem_1fr_2.25rem] items-center gap-3"
        indicatorClassName="rounded-full bg-chart-3"
        trackClassName="h-2"
        value={score.overall}
      >
        <ProgressLabel className="font-semibold text-chart-3">Overall</ProgressLabel>
        {/* The primitive renders its track after its children, so the value is ordered last. */}
        <ProgressValue className="order-last font-semibold text-foreground">
          {() => value}
        </ProgressValue>
      </Progress>
    </section>
  );
}
