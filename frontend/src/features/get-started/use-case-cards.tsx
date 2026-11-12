import { StatusBadge } from "@/components/ui/status-badge";
import type { UseCase } from "@/gen/dotcore/catalog/v1/recommendation_pb";

import type { UseCaseProposal } from "./get-started-proposals";
import { useCaseCopy } from "./use-case-copy";

/**
 * The use cases Get started offers, one pressable card each. A card is marked ready when the site
 * already serves its use case.
 */
export function UseCaseCards({
  onSelect,
  proposals,
  selected,
}: {
  onSelect: (useCase: UseCase) => void;
  proposals: readonly UseCaseProposal[];
  selected: UseCase;
}) {
  return (
    <div
      aria-label="Use case"
      className="grid grid-cols-[repeat(auto-fit,minmax(190px,1fr))] gap-2.5"
      role="group"
    >
      {proposals.map(({ ready, useCase }) => {
        const { description, icon: Icon, label } = useCaseCopy[useCase];
        return (
          <button
            aria-pressed={useCase === selected}
            className="grid gap-0.75 rounded-xl border-[1.5px] border-border bg-card px-3.5 py-3 text-left outline-none hover:border-foreground/25 focus-visible:ring-3 focus-visible:ring-ring/50 aria-pressed:border-primary aria-pressed:bg-info"
            key={useCase}
            onClick={() => onSelect(useCase)}
            type="button"
          >
            <span className="flex items-center gap-2 text-sm font-semibold">
              <Icon aria-hidden="true" className="size-4" />
              {label}
              {ready ? (
                <span className="ml-auto">
                  <StatusBadge status={{ label: "Ready", tone: "success" }} />
                </span>
              ) : null}
            </span>
            <span className="text-[12.5px] text-muted-foreground">{description}</span>
          </button>
        );
      })}
    </div>
  );
}
