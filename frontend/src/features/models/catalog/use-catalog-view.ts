import { useCallback, useEffect, useMemo } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import type { ModelClient } from "@/api/model-client";
import type { SiteClient } from "@/api/site-client";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { useConfiguredExecutionTargets } from "@/features/deployments/use-configured-execution-targets";
import type { ModelsPolling } from "@/features/models/polling/models-polling-context";
import { projectModelRows } from "@/features/models/profiles/model-row";
import { buildCatalogTable } from "@/features/models/profiles/model-table-model";
import {
  catalogLocation,
  catalogRunsOn,
  catalogSizingView,
  hiddenByFitNote,
} from "@/features/models/profiles/profile-availability";
import {
  defaultMemoryBasis,
  type MemoryBasis,
} from "@/features/models/profiles/profile-compatibility";
import {
  applyMemoryBasis,
  applySizingScenario,
  parseMemoryBasis,
  parseSizingScenario,
  type SizingScenario,
  sizingScenarioRequest,
} from "@/features/models/profiles/sizing-scenario";
import {
  allModels,
  useProfileCompatibility,
} from "@/features/models/profiles/use-profile-compatibility";
import { useSizedForChipSettings } from "@/features/models/profiles/use-sized-for-chip-settings";
import type { EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import type { HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";

import { catalogRunsOnTableUi, catalogTableUi } from "./catalog-table";
import { catalogViewCodec, type CatalogViewState } from "./catalog-view-state";

/**
 * The Catalog table's view and the site read it shows: the table view, the Runs on chip's sizing
 * scenario and memory basis, all kept in the URL, and the whole catalog's compatibility and memory
 * estimates on the site's execution targets, from which the table's rows are built.
 */
export function useCatalogView({
  client,
  engineReleases,
  hardwareClasses,
  siteClient,
  siteReadsActive,
  snapshot,
}: CatalogViewOptions) {
  const location = useLocation();
  const navigate = useNavigate();
  const listConfiguredExecutionTargets = useCallback(
    () => siteClient.listConfiguredExecutionTargets({}),
    [siteClient],
  );
  const readProfileCompatibility = client.model.listModelProfileCompatibility;
  const listProfileCompatibility = useMemo(
    () =>
      readProfileCompatibility
        ? (modelId: string, scenario?: SizingScenario) =>
            readProfileCompatibility({
              ...(modelId === allModels ? {} : { modelId }),
              ...(scenario ? { scenario: sizingScenarioRequest(scenario) } : {}),
            })
        : undefined,
    [readProfileCompatibility],
  );
  // The Runs on chip's sizing scenario lives in the URL beside the table view; absent, or without
  // the chip, profiles are sized from their defaults.
  const urlScenario = useMemo(() => parseSizingScenario(location.search), [location.search]);
  const runsOnInUrl = useMemo(
    () =>
      catalogViewCodec.parse(location.search).predicates.some(({ field }) => field === "runsOn"),
    [location.search],
  );
  const scenario = runsOnInUrl ? urlScenario : undefined;
  const setScenario = useCallback(
    (next: SizingScenario | undefined) =>
      void navigate(
        { pathname: location.pathname, search: applySizingScenario(location.search, next) },
        { replace: true, state: location.state as unknown },
      ),
    [location, navigate],
  );
  // The memory basis Runs on judges fits on lives in the URL beside the scenario; absent, or without
  // the chip, profiles are judged on free memory.
  const urlBasis = useMemo(() => parseMemoryBasis(location.search), [location.search]);
  const basis = runsOnInUrl ? urlBasis : defaultMemoryBasis;
  const setBasis = useCallback(
    (next: MemoryBasis) =>
      void navigate(
        { pathname: location.pathname, search: applyMemoryBasis(location.search, next) },
        { replace: true, state: location.state as unknown },
      ),
    [location, navigate],
  );

  // The table, the catalog model panel and its profile screens share one read of the whole
  // catalog's compatibility and memory estimates. It pauses while the deploy dialog, which reads
  // its own compatibility, is open.
  const siteCompatibility = useProfileCompatibility(
    listProfileCompatibility,
    allModels,
    siteReadsActive,
    scenario,
  );
  const { executionTargets: siteTargets } = useConfiguredExecutionTargets(
    listConfiguredExecutionTargets,
    siteReadsActive,
  );
  // Runs on judges memory fit from the shown read, so a new scenario's read keeps the previous
  // verdicts until it lands; after it failed, Runs on judges support alone.
  const catalogRunsOnFilter = useMemo(
    () =>
      siteCompatibility.compatibility
        ? catalogRunsOn(siteCompatibility.compatibility, siteTargets, {
            basis,
            fit: !siteCompatibility.sizingFailed,
          })
        : undefined,
    [basis, siteCompatibility.compatibility, siteCompatibility.sizingFailed, siteTargets],
  );
  // A scenario and a memory basis belong to the Runs on chip, so removing the chip, or a site that
  // settles without offering it, drops both from the URL in one navigation.
  const runsOnOffered = !siteCompatibility.compatibility || Boolean(catalogRunsOnFilter);
  const urlSettings = Boolean(urlScenario) || urlBasis !== defaultMemoryBasis;
  useEffect(() => {
    if (urlSettings && !(runsOnInUrl && runsOnOffered))
      void navigate(
        {
          pathname: location.pathname,
          search: applyMemoryBasis(
            applySizingScenario(location.search, undefined),
            defaultMemoryBasis,
          ),
        },
        { replace: true, state: location.state as unknown },
      );
  }, [location, navigate, runsOnInUrl, runsOnOffered, urlSettings]);
  // Without a known Runs on field, the table lists every model with no default chip.
  const catalogUi = catalogRunsOnFilter ? catalogRunsOnTableUi : catalogTableUi;
  const view = useDataTableView(catalogUi.codec);
  // A location that names its chips, even none, is written with the Runs on codec, which records a
  // removed default chip as `f=`; the plain codec has no default chip to remove and would drop it.
  const applyView = useCallback(
    (search: string, state: CatalogViewState) =>
      (new URLSearchParams(search).has("f") ? catalogViewCodec : catalogUi.codec).apply(
        search,
        state,
      ),
    [catalogUi.codec],
  );
  const profileEstimates = siteCompatibility.estimates;
  // The first read shows its placeholder at once; a new scenario's keeps the last estimates until
  // its loading feedback is due.
  const scenarioPendingVisible = useDelayedLoading(profileEstimates.pending);
  const memoryPending =
    profileEstimates.pending && (siteCompatibility.loading || scenarioPendingVisible);
  const runsOnChip = view.state.predicates.find(({ field }) => field === "runsOn");
  const catalogLocationScope = useMemo(
    () => catalogLocation(catalogRunsOnFilter, runsOnChip?.values ?? []),
    [catalogRunsOnFilter, runsOnChip],
  );
  const sizingView = useMemo(
    () =>
      siteCompatibility.compatibility
        ? catalogSizingView(
            siteCompatibility.compatibility,
            siteCompatibility.sizing,
            catalogLocationScope,
            scenario,
            basis,
          )
        : undefined,
    [
      basis,
      catalogLocationScope,
      scenario,
      siteCompatibility.compatibility,
      siteCompatibility.sizing,
    ],
  );
  const modelRows = useMemo(
    () =>
      snapshot
        ? projectModelRows({
            deployments: snapshot.runningModels,
            engineReleases: engineReleases ?? [],
            hardwareClasses: hardwareClasses ?? [],
            estimates: profileEstimates.estimates,
            models: snapshot.availableModels,
            sizing: sizingView,
          })
        : [],
    [engineReleases, hardwareClasses, profileEstimates.estimates, sizingView, snapshot],
  );
  const catalogTable = useMemo(
    () =>
      buildCatalogTable({
        estimatesPending: memoryPending,
        rows: modelRows,
        runsOn: catalogRunsOnFilter,
        showFit: catalogLocationScope.scopes.every(({ kind }) => kind === "node"),
        view: view.state,
      }),
    [catalogLocationScope, catalogRunsOnFilter, memoryPending, modelRows, view.state],
  );
  // Why the Runs on filter isn't offered, when the read settled without it.
  const runsOnNotice =
    siteCompatibility.compatibility && !catalogRunsOnFilter
      ? "Runs on isn't applied: this site has no model execution target."
      : !siteCompatibility.compatibility && siteCompatibility.error
        ? "Runs on isn't available: the compatibility read failed."
        : undefined;
  // Why the scenario's estimates, and the memory fit of Runs on with them, are missing.
  const sizingNotice = siteCompatibility.sizingFailed
    ? "Runs on doesn't check memory: the sizing read failed."
    : undefined;
  const removeRunsOn = () =>
    view.set({
      ...view.state,
      predicates: view.state.predicates.filter((predicate) => predicate.field !== "runsOn"),
    });
  const hiddenNote = hiddenByFitNote(catalogTable.hiddenByFit);
  const chipSettings = useSizedForChipSettings(
    catalogRunsOnFilter ? { onChange: setScenario, start: undefined, value: scenario } : undefined,
    catalogRunsOnFilter
      ? { onChange: setBasis, start: defaultMemoryBasis, value: basis }
      : undefined,
  );
  // The view, the scenario and the memory basis share the URL, so Reset writes them in one
  // navigation; several would each start from the same location and the later would undo the
  // earlier.
  const resetView = () =>
    void navigate(
      {
        pathname: location.pathname,
        search: applyMemoryBasis(
          applySizingScenario(
            catalogUi.codec.apply(location.search, catalogUi.codec.defaultView),
            undefined,
          ),
          defaultMemoryBasis,
        ),
      },
      { replace: true, state: location.state as unknown },
    );

  return {
    applyView,
    basis,
    catalogLocationScope,
    catalogRunsOnFilter,
    catalogTable,
    catalogUi,
    chipSettings,
    hiddenNote,
    listConfiguredExecutionTargets,
    listProfileCompatibility,
    memoryPending,
    modelRows,
    profileEstimates,
    removeRunsOn,
    resetView,
    runsOnNotice,
    scenario,
    siteCompatibility,
    siteTargets,
    sizingNotice,
    view,
  };
}

export interface CatalogViewOptions {
  client: ModelClient;
  engineReleases?: EngineRelease[];
  hardwareClasses?: HardwareClass[];
  siteClient: SiteClient;
  /** Whether the site read runs; it pauses while the deploy dialog reads its own. */
  siteReadsActive: boolean;
  snapshot: ModelsPolling["snapshot"];
}
