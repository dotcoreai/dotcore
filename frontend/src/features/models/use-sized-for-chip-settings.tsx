import { useMemo } from "react";

import type { DataTableChipSetting } from "@/components/data-table/data-table-toolbar";
import { DropdownMenuSeparator } from "@/components/ui/dropdown-menu";

import { CatalogMemoryBasis, CatalogSizedFor } from "./catalog-sized-for";
import { defaultMemoryBasis, type MemoryBasis } from "./profile-compatibility";
import { formatSizingScenario, sameSizingScenario, type SizingScenario } from "./sizing-scenario";

/** One setting of the Runs on chip: its value, how to change it, and the value the table starts with. */
export interface ChipSettingState<T> {
  onChange: (value: T) => void;
  start: T;
  value: T;
}

/**
 * The Runs on chip's settings for any table with the chip: the "Sized for" and memory basis menu
 * sections, the scenario and a Total basis as the chip's summary, a reset to the values the table
 * starts with while either differs, and a clear back to profile defaults and free memory for when
 * the chip is removed. Undefined without setters, such as when no Runs on filter is offered to
 * carry them.
 */
export function useSizedForChipSettings(
  sizedFor: ChipSettingState<SizingScenario | undefined> | undefined,
  basis: ChipSettingState<MemoryBasis> | undefined,
): { runsOn: DataTableChipSetting } | undefined {
  const scenario = sizedFor?.value;
  const onScenarioChange = sizedFor?.onChange;
  const startScenario = sizedFor?.start;
  const memoryBasis = basis?.value;
  const onBasisChange = basis?.onChange;
  const startBasis = basis?.start;
  return useMemo(() => {
    if (!onScenarioChange || !onBasisChange || !memoryBasis) return undefined;

    const scenarioChanged = !sameSizingScenario(scenario, startScenario);
    const basisChanged = memoryBasis !== startBasis;
    const summary = [
      scenario ? formatSizingScenario(scenario) : undefined,
      memoryBasis === "total" ? "Total memory" : undefined,
    ].filter(Boolean);
    return {
      runsOn: {
        clear:
          scenario || memoryBasis !== defaultMemoryBasis
            ? () => {
                if (scenario) onScenarioChange(undefined);
                if (memoryBasis !== defaultMemoryBasis) onBasisChange(defaultMemoryBasis);
              }
            : undefined,
        menu: (
          <>
            <CatalogSizedFor onChange={onScenarioChange} scenario={scenario} />
            <DropdownMenuSeparator />
            <CatalogMemoryBasis basis={memoryBasis} onChange={onBasisChange} />
          </>
        ),
        reset:
          scenarioChanged || basisChanged
            ? () => {
                if (scenarioChanged) onScenarioChange(startScenario);
                if (basisChanged && startBasis) onBasisChange(startBasis);
              }
            : undefined,
        summary: summary.length ? summary.join(" · ") : undefined,
      },
    };
  }, [memoryBasis, onBasisChange, onScenarioChange, scenario, startBasis, startScenario]);
}
