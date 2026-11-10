import { useCallback, useMemo, useState } from "react";

import type { ModelsPolling } from "@/features/models/polling/models-polling-context";
import { projectModelRows } from "@/features/models/profiles/model-row";
import {
  type catalogLocation,
  catalogSizingView,
} from "@/features/models/profiles/profile-availability";
import type { MemoryBasis } from "@/features/models/profiles/profile-compatibility";
import type { CatalogChips } from "@/features/models/profiles/profile-table-seed";
import {
  sameSizingScenario,
  type SizingScenario,
} from "@/features/models/profiles/sizing-scenario";
import {
  type ProfileCompatibilityLoader,
  type ProfileCompatibilityState,
  useProfileCompatibility,
} from "@/features/models/profiles/use-profile-compatibility";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import type { Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { ConfiguredExecutionTarget } from "@/gen/dotcore/site/v1/service_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

import { catalogViewCodec } from "./catalog-view-state";

/**
 * The catalog model panel's own sizing: the chips, "Sized for" and memory basis it copied from the
 * catalog when it opened, the read of its model under its own "Sized for", and the panel's model
 * row under them. While they match the catalog's, the panel shows the site read.
 */
export function useCatalogPanelSizing({
  basis,
  catalogLocationScope,
  engineReleases,
  hardwareClasses,
  listProfileCompatibility,
  locationSearch,
  memoryPending,
  panelModelId,
  scenario,
  selectedModel,
  siteCompatibility,
  siteReadsActive,
  siteTargets,
  snapshot,
}: CatalogPanelSizingOptions) {
  // The catalog's chips and "Sized for" the model panel's profile table starts from, taken once
  // per open of a model, so later catalog changes and profile screens leave them alone.
  const [panelChips, setPanelChips] = useState<{
    basis: MemoryBasis;
    chips: CatalogChips;
    modelId: string;
    scenario: SizingScenario | undefined;
    startBasis: MemoryBasis;
    startScenario: SizingScenario | undefined;
  }>();
  if (panelChips?.modelId !== panelModelId)
    setPanelChips(
      panelModelId === undefined
        ? undefined
        : {
            basis,
            chips: catalogViewCodec.parse(locationSearch).predicates,
            modelId: panelModelId,
            scenario,
            startBasis: basis,
            startScenario: scenario,
          },
    );
  const panelBasis = panelChips?.basis ?? basis;
  const setPanelBasis = useCallback(
    (next: MemoryBasis) => setPanelChips((current) => current && { ...current, basis: next }),
    [],
  );
  const panelScenario = panelChips?.scenario;
  const setPanelScenario = useCallback(
    (next: SizingScenario | undefined) =>
      setPanelChips((current) => current && { ...current, scenario: next }),
    [],
  );
  // While the panel's "Sized for" matches the catalog's, it shows the site read; otherwise it reads
  // its model under its own, keeping the site read's verdicts in view until that read lands.
  const panelOwnScenario = Boolean(panelChips) && !sameSizingScenario(panelScenario, scenario);
  const panelCompatibility = useProfileCompatibility(
    listProfileCompatibility,
    panelModelId ?? "",
    siteReadsActive && panelOwnScenario,
    panelScenario,
  );
  const panelRead = panelOwnScenario ? panelCompatibility.compatibility : undefined;
  const panelReadShown = panelRead ? panelCompatibility : siteCompatibility;
  const catalogCompatibility = useMemo(
    () =>
      listProfileCompatibility
        ? {
            basis: panelBasis,
            compatibility: panelReadShown.compatibility,
            error: panelReadShown.error,
            loading: panelReadShown.loading,
            onBasisChange: setPanelBasis,
            onScenarioChange: setPanelScenario,
            scenario: panelScenario,
            sizingFailed: panelReadShown.sizingFailed,
            startBasis: panelChips?.startBasis,
            startScenario: panelChips?.startScenario,
            targets: siteTargets,
          }
        : undefined,
    [
      listProfileCompatibility,
      panelBasis,
      panelChips?.startBasis,
      setPanelBasis,
      panelReadShown.compatibility,
      panelReadShown.error,
      panelReadShown.loading,
      panelReadShown.sizingFailed,
      panelChips?.startScenario,
      panelScenario,
      setPanelScenario,
      siteTargets,
    ],
  );
  // The panel keeps showing the shown estimates until its read's loading feedback is due.
  const panelPendingVisible = useDelayedLoading(panelCompatibility.estimates.pending);
  const panelMemoryPending = panelOwnScenario ? panelPendingVisible : memoryPending;
  // The panel's row under its own "Sized for", once its read landed, or under its own memory basis
  // from the shown read.
  const panelOwnBasis = Boolean(panelChips) && panelBasis !== basis;
  const panelRowRead = panelRead
    ? panelCompatibility
    : panelOwnBasis
      ? siteCompatibility
      : undefined;
  const panelRowCompatibility = panelRowRead?.compatibility;
  const panelRowEstimates = panelRowRead?.estimates.estimates;
  const panelRowSizing = panelRowRead?.sizing;
  const panelRow = useMemo(() => {
    if (!panelRowCompatibility || !panelRowEstimates || !snapshot || !selectedModel)
      return undefined;
    return projectModelRows({
      deployments: snapshot.runningModels,
      engineReleases: engineReleases ?? [],
      hardwareClasses: hardwareClasses ?? [],
      estimates: panelRowEstimates,
      models: [selectedModel],
      sizing: catalogSizingView(
        panelRowCompatibility,
        panelRowSizing,
        catalogLocationScope,
        panelScenario,
        panelBasis,
      ),
    })[0];
  }, [
    catalogLocationScope,
    engineReleases,
    hardwareClasses,
    panelBasis,
    panelRowCompatibility,
    panelRowEstimates,
    panelRowSizing,
    panelScenario,
    selectedModel,
    snapshot,
  ]);

  return {
    catalogCompatibility,
    panelBasis,
    panelChips: panelChips?.chips,
    panelMemoryPending,
    panelRow,
    panelScenario,
  };
}

export interface CatalogPanelSizingOptions {
  basis: MemoryBasis;
  catalogLocationScope: ReturnType<typeof catalogLocation>;
  engineReleases?: EngineRelease[];
  hardwareClasses?: HardwareClass[];
  listProfileCompatibility?: ProfileCompatibilityLoader;
  /** The page's URL search, whose chips the panel copies when it opens. */
  locationSearch: string;
  memoryPending: boolean;
  /** The catalog model whose panel is open; absent without one. */
  panelModelId?: string;
  scenario?: SizingScenario;
  selectedModel?: Model;
  siteCompatibility: ProfileCompatibilityState;
  siteReadsActive: boolean;
  siteTargets: ConfiguredExecutionTarget[];
  snapshot: ModelsPolling["snapshot"];
}
