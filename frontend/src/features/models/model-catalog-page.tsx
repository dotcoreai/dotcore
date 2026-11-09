import { create } from "@bufbuild/protobuf";
import { ConnectError } from "@connectrpc/connect";
import { Plus } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useLocation, useNavigate } from "react-router-dom";

import { type ModelClient, modelClient } from "@/api/model-client";
import { type SiteClient, siteClient as defaultSiteClient } from "@/api/site-client";
import { usePanelNavigation } from "@/app/console-layout";
import { CatalogSectionIcon } from "@/app/console-section-icons";
import { useWatchedApplicationDeployments, useWatchedRoutes } from "@/app/resource-watch";
import { DataTable } from "@/components/data-table/data-table";
import { DataTableToolbar } from "@/components/data-table/data-table-toolbar";
import { useDataTableView } from "@/components/data-table/use-data-table-view";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { Notice } from "@/components/ui/notice";
import { PageActions, PageHeader, PageHeaderHeading } from "@/components/ui/page-header";
import { useConfiguredExecutionTargets } from "@/features/deployments/use-configured-execution-targets";
import { ModelDeployDialog } from "@/features/models/model-deploy-dialog";
import { CatalogModelInspector } from "@/features/models/model-inspector";
import { modelDeploymentPresentationName } from "@/features/models/model-projection";
import { type EngineRelease } from "@/gen/dotcore/catalog/v1/engine_release_pb";
import { type HardwareClass } from "@/gen/dotcore/catalog/v1/hardware_class_pb";
import { type Model } from "@/gen/dotcore/catalog/v1/model_pb";
import type { UpstreamModelTemplate } from "@/gen/dotcore/catalog/v1/upstream_model_template_pb";
import type { InferenceRouteView } from "@/gen/dotcore/inference/v1/routes_pb";
import {
  type ModelDeployment,
  type ResolvedLocalModelDeploymentSnapshot,
} from "@/gen/dotcore/model/v1/deployment_pb";
import {
  type CreateLocalModelDeploymentRequest,
  CreateLocalModelDeploymentRequestSchema,
  EstimateLocalModelResourcesRequestSchema,
  type LocalModelDeploymentAdvisory,
  LocalModelDeploymentConfigurationSchema,
  type ModelDeploymentExposure,
} from "@/gen/dotcore/model/v1/service_pb";
import { AcceleratorMemoryArchitecture } from "@/gen/dotcore/observation/v1/observation_pb";
import { useDelayedLoading } from "@/hooks/use-delayed-loading";
import { usePanelPendingNavigation } from "@/panels/panel-navigation-pending";
import {
  activePanel,
  appendPanel,
  appendPanelLocation,
  canonicalizePanelLocation,
  deepensPanelStack,
  isSamePanelEntry,
  type PanelEntry,
  panelPagePath,
  popPanel,
  selectPanel,
  serializePanelStack,
} from "@/panels/panel-stack";
import { panelTitle } from "@/panels/panel-titles";
import { usePublishPanelTitles } from "@/panels/panel-titles-context";

import { catalogModelDeployments, projectModelRows } from "./catalog-model-row";
import {
  catalogLocation,
  catalogRunsOn,
  catalogSizingView,
  hiddenByFitNote,
} from "./catalog-profile-compatibility";
import { catalogRunsOnTableUi, catalogTableUi } from "./catalog-table";
import { buildCatalogTable } from "./catalog-table-model";
import { catalogViewCodec, type CatalogViewState } from "./catalog-view-state";
import { type RouteNameConflict } from "./deployment-exposure";
import { describeMutationError, routeConflictFromError } from "./model-mutation-errors";
import { useSharedModelsPolling } from "./models-polling-context";
import { defaultMemoryBasis, type MemoryBasis } from "./profile-compatibility";
import type { CatalogChips } from "./profile-table-seed";
import {
  applyMemoryBasis,
  applySizingScenario,
  parseMemoryBasis,
  parseSizingScenario,
  sameSizingScenario,
  type SizingScenario,
  sizingScenarioRequest,
} from "./sizing-scenario";
import {
  toUpstreamConfiguration,
  UpstreamDeployDialog,
  type UpstreamFormValues,
} from "./upstream-model";
import { allModels, useProfileCompatibility } from "./use-profile-compatibility";
import { useSizedForChipSettings } from "./use-sized-for-chip-settings";

export interface ModelCatalogPageProps {
  client?: ModelClient;
  siteClient?: SiteClient;
}

/**
 * The model Catalog page: the Catalog table with its toolbar, the "Connect upstream" action,
 * and their inspectors. The table view lives in the URL. It owns every deploy; an accepted
 * deployment opens its panel on the Models page.
 */
export function ModelCatalogPage({
  client = modelClient,
  siteClient = defaultSiteClient,
}: ModelCatalogPageProps) {
  const location = useLocation();
  const navigate = useNavigate();
  const panelNavigation = usePanelNavigation();
  const [selection, setSelection] = useState<CatalogSelection>();
  const canonical = useMemo(
    () => canonicalizePanelLocation({ pathname: location.pathname, search: location.search }),
    [location.pathname, location.search],
  );
  const urlEntry = activePanel(canonical.search);
  const urlSelection = catalogSelectionFromEntry(urlEntry);
  const [upstreamTemplates, setUpstreamTemplates] = useState<UpstreamModelTemplate[]>();
  const [engineReleases, setEngineReleases] = useState<EngineRelease[]>();
  const [hardwareClasses, setHardwareClasses] = useState<HardwareClass[]>();
  const [deployTemplate, setDeployTemplate] = useState<UpstreamModelTemplate>();
  const [deployModel, setDeployModel] = useState<Model>();
  const [deployProfileId, setDeployProfileId] = useState<string>();
  // The catalog's chips the deploy dialog's profile table starts from, as the model panel took them.
  const [deployChips, setDeployChips] = useState<CatalogChips>();
  const [deployScenario, setDeployScenario] = useState<SizingScenario>();
  const [deployBasis, setDeployBasis] = useState<MemoryBasis>(defaultMemoryBasis);
  const [upstreamDeployPending, setUpstreamDeployPending] = useState(false);
  const [upstreamError, setUpstreamError] = useState<string>();
  const [routeConflict, setRouteConflict] = useState<RouteNameConflict>();
  const [deployError, setDeployError] = useState<DeployError>();
  const { applyDeploymentOverlay, error: pollingError, snapshot } = useSharedModelsPolling();
  const loadingFeedbackVisible = useDelayedLoading(!snapshot);
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
  const siteReadsActive = !deployModel;
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
  const noneRunEmpty =
    catalogRunsOnFilter && catalogTable.noneRun && !catalogTable.rows.length ? (
      <div className="flex flex-col items-center gap-1">
        <p>No model matches the filter.</p>
        <Button onClick={removeRunsOn} size="xs" type="button" variant="link">
          Show all models
        </Button>
      </div>
    ) : undefined;
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
  const [pendingActions, setPendingActions] = useState<ReadonlySet<string>>(() => new Set());
  const pendingActionsRef = useRef<Set<string>>(new Set());
  const openerRef = useRef<HTMLElement | null>(null);
  const deployDialogOpenerRef = useRef<HTMLButtonElement>(null);
  const headingRef = useRef<HTMLHeadingElement>(null);
  const deployTemplateRef = useRef<UpstreamModelTemplate | undefined>(undefined);
  const upstreamDeployPendingRef = useRef(false);
  const selectionRef = useRef<CatalogSelection | undefined>(undefined);
  const pendingSelectionRef = useRef<CatalogSelection | undefined>(undefined);
  const shouldRestoreHeadingFocus = useRef(false);
  const mountedRef = useRef(false);
  const closeSelection = useCallback(() => {
    setDeployModel(undefined);
    setDeployProfileId(undefined);
    pendingSelectionRef.current = undefined;
    selectionRef.current = undefined;
    setDeployError(undefined);
    setUpstreamError(undefined);
    setSelection(undefined);
    panelNavigation.close();
  }, [panelNavigation]);

  useEffect(() => {
    mountedRef.current = true;
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (canonical.recovered || (urlEntry && !urlSelection)) {
      // The shown view wins over the one the location was read with; another page takes none.
      void navigate(
        canonical.pathname === location.pathname
          ? { ...canonical, search: applyView(canonical.search, view.state) }
          : canonical,
        { replace: true },
      );
      return;
    }

    if (pendingSelectionRef.current) {
      if (isSameSelection(pendingSelectionRef.current, urlSelection)) {
        pendingSelectionRef.current = undefined;
      } else {
        return;
      }
    }

    if (!isSameSelection(selectionRef.current, urlSelection)) {
      selectionRef.current = urlSelection;
      setDeployError(undefined);
      setUpstreamError(undefined);
      setSelection(urlSelection);
    }
  }, [applyView, canonical, location.pathname, navigate, urlEntry, urlSelection, view.state]);

  useEffect(() => {
    void client.catalog
      .listHardwareClasses({})
      .then((response) => setHardwareClasses(response.hardwareClasses))
      .catch(() => setHardwareClasses([]));
  }, [client]);

  useEffect(() => {
    void client.catalog
      .listEngineReleases({})
      .then((response) => setEngineReleases(response.engineReleases))
      .catch(() => setEngineReleases([]));
  }, [client]);

  useEffect(() => {
    let active = true;
    void client.catalog
      .listUpstreamModelTemplates({})
      .then((response) => {
        if (active) setUpstreamTemplates(response.upstreamModelTemplates);
      })
      .catch(() => {
        if (active) setUpstreamTemplates([]);
      });
    return () => {
      active = false;
    };
  }, [client]);

  const routeViews = useWatchedRoutes()?.value?.routes ?? noRouteViews;
  // Application deployments name their memory in the deploy dialog's preview, so they are watched
  // only while it is open.
  const applications = useWatchedApplicationDeployments({ enabled: Boolean(deployModel) })?.value
    ?.deployments;

  const selectionError =
    deployError && isSameSelection(deployError.selection, selection)
      ? deployError.message
      : undefined;
  const selectedModel =
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? snapshot?.availableModels.find((model) => model.modelId === selection.modelId)
      : undefined;
  // The catalog's chips and "Sized for" the model panel's profile table starts from, taken once
  // per open of a model, so later catalog changes and profile screens leave them alone.
  const panelModelId =
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? selection.modelId
      : undefined;
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
            chips: catalogViewCodec.parse(location.search).predicates,
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
  usePublishPanelTitles([
    ...(snapshot?.availableModels ?? []).map((model) =>
      panelTitle("model-catalog", "catalog", model.modelId, model.displayName),
    ),
    ...(snapshot?.runningModels ?? []).map((deployment) =>
      panelTitle(
        "models",
        "deployment",
        deployment.modelDeploymentId,
        modelDeploymentPresentationName(deployment),
      ),
    ),
  ]);
  usePanelPendingNavigation(
    selection?.kind === "catalog" || selection?.kind === "catalog-profile"
      ? pendingActions.has(catalogPendingKey(selection.modelId))
      : false,
    "An inspector operation is pending. Inspector navigation is unavailable.",
  );

  useEffect(() => {
    if (!selection || !snapshot) {
      return;
    }

    if (
      selection.kind === "catalog-profile" &&
      selectedModel &&
      !selectedModel.profiles.some((profile) => profile.profileId === selection.profileId)
    ) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          void navigate(
            {
              pathname: panelPagePath("model-catalog"),
              search: applyView(popPanel(location.search), view.state),
            },
            { replace: true },
          );
        }
      });
      return;
    }

    if ((selection.kind === "catalog" || selection.kind === "catalog-profile") && !selectedModel) {
      queueMicrotask(() => {
        if (isSameSelection(selectionRef.current, selection)) {
          openerRef.current = headingRef.current;
          shouldRestoreHeadingFocus.current = true;
          setDeployModel(undefined);
          closeSelection();
        }
      });
    }
  }, [
    applyView,
    closeSelection,
    selectedModel,
    selection,
    snapshot,
    location.search,
    navigate,
    view.state,
  ]);

  useEffect(() => {
    if (!selection && shouldRestoreHeadingFocus.current) {
      headingRef.current?.focus();
      shouldRestoreHeadingFocus.current = false;
    }
  }, [selection]);

  const updatePending = (key: string, pending: boolean) => {
    if (pending) {
      pendingActionsRef.current.add(key);
    } else {
      pendingActionsRef.current.delete(key);
    }

    setPendingActions(new Set(pendingActionsRef.current));
  };

  /**
   * Opens an accepted deployment's panel on the Models page as a new history entry, so Back returns
   * to the catalog inspector the deploy started from.
   */
  const openAcceptedDeployment = (modelDeploymentId: string) => {
    // A deploy accepted after the operator left the page keeps them where they went.
    if (!mountedRef.current) return;

    panelNavigation.navigatePanel(
      {
        pathname: panelPagePath("models"),
        // The pathname changes, so the Catalog view parameters stay behind.
        search: serializePanelStack("", [
          { page: "models", resourceId: modelDeploymentId, screen: "deployment" },
        ]),
      },
      { bypassPendingGuard: true },
    );
  };

  const load = async (
    model: Model,
    profileId: string,
    configuration: CreateLocalModelDeploymentRequest["configuration"],
    placement: CreateLocalModelDeploymentRequest["placement"],
    exposure: ModelDeploymentExposure,
    name: string,
  ) => {
    const initiatingSelection = selectionRef.current;
    if (!initiatingSelection) return;
    const pendingKey = catalogPendingKey(model.modelId);
    if (pendingActionsRef.current.has(pendingKey)) {
      return;
    }

    setDeployError(undefined);
    setRouteConflict(undefined);
    updatePending(pendingKey, true);
    try {
      if (!client.model.createLocalModelDeployment) {
        throw new Error(
          "Deploy is unavailable because the model service does not support explicit local deployments.",
        );
      }
      const response = await client.model.createLocalModelDeployment({
        ...create(CreateLocalModelDeploymentRequestSchema, {
          modelId: model.modelId,
          profileId,
          placement,
          configuration,
        }),
        exposure,
        modelDeploymentName: name,
      });
      if (!isSameSelection(selectionRef.current, initiatingSelection)) {
        return;
      }

      const deployment = response.deployment;
      if (!deployment?.modelDeploymentId) {
        setDeployError({
          message: "Deploy response protocol error: missing accepted deployment ID.",
          selection: initiatingSelection,
        });
        return;
      }

      setDeployModel(undefined);
      setDeployProfileId(undefined);
      applyDeploymentOverlay(deployment);
      openAcceptedDeployment(deployment.modelDeploymentId);
    } catch (mutationError) {
      const conflict = routeConflictFromError(mutationError);
      if (conflict) {
        setRouteConflict(conflict);
      } else if (isSameSelection(selectionRef.current, initiatingSelection)) {
        setDeployError({
          message: describeMutationError("Deploy", mutationError),
          selection: initiatingSelection,
        });
      }
    } finally {
      updatePending(pendingKey, false);
    }
  };

  const deployUpstream = async (template: UpstreamModelTemplate, values: UpstreamFormValues) => {
    if (upstreamDeployPendingRef.current) return;
    upstreamDeployPendingRef.current = true;
    setUpstreamDeployPending(true);
    setUpstreamError(undefined);
    setRouteConflict(undefined);
    try {
      const response = await client.model.createModelDeployment({
        modelDeploymentName: values.name?.trim(),
        source: {
          case: "upstream",
          value: {
            upstreamModelTemplateId: template.upstreamModelTemplateId,
            desiredConfiguration: toUpstreamConfiguration(values),
          },
        },
        exposure:
          values.exposure === "direct"
            ? { kind: { case: "direct", value: { routeName: values.routeName?.trim() ?? "" } } }
            : { kind: { case: "hidden", value: {} } },
      });
      const deployment = response.deployment;
      if (!deployment?.modelDeploymentId.trim()) {
        throw new Error(
          "Deploy response protocol error: service returned no deployment ID. Keep the dialog open and retry.",
        );
      }
      if (deployTemplateRef.current?.upstreamModelTemplateId !== template.upstreamModelTemplateId) {
        return;
      }
      deployTemplateRef.current = undefined;
      setDeployTemplate(undefined);
      applyDeploymentOverlay(deployment);
      openAcceptedDeployment(deployment.modelDeploymentId);
    } catch (error) {
      const conflict = routeConflictFromError(error);
      if (conflict) {
        setRouteConflict(conflict);
      } else {
        setUpstreamError(ConnectError.from(error).rawMessage || "Deploy was rejected.");
      }
    } finally {
      upstreamDeployPendingRef.current = false;
      setUpstreamDeployPending(false);
    }
  };

  // The dialog starts from the chips the model panel copied when it opened, not the catalog's
  // current ones.
  const startDeploy = (
    model: Model,
    profileId: string | undefined,
    chips: CatalogChips,
    deployScenario: SizingScenario | undefined,
    basis: MemoryBasis,
  ) => {
    setDeployProfileId(profileId);
    setDeployChips(chips);
    setDeployScenario(deployScenario);
    setDeployBasis(basis);
    setDeployModel(model);
  };

  const select = (nextSelection: CatalogSelection, opener: HTMLElement | null) => {
    openerRef.current = opener;
    pendingSelectionRef.current = nextSelection;
    selectionRef.current = nextSelection;
    setDeployError(undefined);
    setUpstreamError(undefined);
    setSelection(nextSelection);
    // A profile opens above its catalog model, so Back returns to the model.
    const search =
      nextSelection.kind === "catalog-profile"
        ? appendPanel(
            selectPanel(location.search, {
              page: "model-catalog",
              resourceId: nextSelection.modelId,
              screen: "catalog",
            }),
            catalogSelectionEntry(nextSelection),
          )
        : selectPanel(location.search, catalogSelectionEntry(nextSelection));
    panelNavigation.navigatePanel(
      { pathname: panelPagePath("model-catalog"), search },
      { replace: !deepensPanelStack(location.search, search) },
    );
  };

  // A deployment the operator came from is the panel beneath, so opening it again returns there.
  const openDeployment = (entry: PanelEntry) => {
    if (panelNavigation.parent && isSamePanelEntry(panelNavigation.parent, entry)) {
      panelNavigation.back();
    } else {
      panelNavigation.navigatePanel(appendPanelLocation(location, entry));
    }
  };

  const onInspectorOpenChange = (open: boolean) => {
    if (!open) {
      setDeployModel(undefined);
      panelNavigation.close();
    }
  };

  return (
    <div className="min-w-0">
      <div className="flex w-full flex-col gap-6">
        <PageHeader>
          <PageHeaderHeading icon={CatalogSectionIcon} ref={headingRef} tabIndex={-1}>
            Catalog
          </PageHeaderHeading>
          {upstreamTemplates?.length ? (
            <PageActions>
              <ConnectUpstreamModelAction
                onSelect={(template) => {
                  setUpstreamError(undefined);
                  setRouteConflict(undefined);
                  deployTemplateRef.current = template;
                  setDeployTemplate(template);
                }}
                templates={upstreamTemplates}
              />
            </PageActions>
          ) : null}
        </PageHeader>

        {loadingFeedbackVisible && !pollingError ? (
          <p className="text-sm text-muted-foreground" role="status">
            Loading models…
          </p>
        ) : null}

        {pollingError ? (
          <p className="text-sm text-destructive" role="alert">
            {pollingError}
          </p>
        ) : null}

        {snapshot && !loadingFeedbackVisible ? (
          snapshot.availableModels.length > 0 ? (
            <section aria-label="Catalog models" className="flex min-w-0 flex-col gap-2">
              <DataTableToolbar
                chipSettings={chipSettings}
                facets={catalogTable.facets}
                onReset={resetView}
                onViewChange={view.set}
                ui={catalogUi}
                view={view.state}
              />
              {runsOnNotice ? (
                <p className="text-xs text-muted-foreground" role="status">
                  {runsOnNotice}
                </p>
              ) : null}
              {sizingNotice ? (
                <p className="text-xs text-muted-foreground" role="status">
                  {sizingNotice}
                </p>
              ) : null}
              <DataTable
                empty={noneRunEmpty}
                footnote={hiddenNote}
                onOpenChild={({ row }, profile, opener) =>
                  select(
                    { kind: "catalog-profile", modelId: row.modelId, profileId: profile.profileId },
                    opener,
                  )
                }
                onOpenRow={({ row }, opener) =>
                  select({ kind: "catalog", modelId: row.modelId }, opener)
                }
                onViewChange={view.set}
                selection={
                  selection?.kind === "catalog"
                    ? { rowId: selection.modelId }
                    : selection?.kind === "catalog-profile"
                      ? { childId: selection.profileId, rowId: selection.modelId }
                      : undefined
                }
                table={catalogTable}
                ui={catalogUi}
                view={view.state}
              />
            </section>
          ) : (
            <Notice role="status">No models are available to deploy.</Notice>
          )
        ) : null}

        {(selection?.kind === "catalog" || selection?.kind === "catalog-profile") &&
        selectedModel ? (
          <CatalogModelInspector
            catalogChips={panelChips?.chips}
            compatibility={catalogCompatibility}
            deployOpenerRef={deployDialogOpenerRef}
            deployments={catalogModelDeployments(
              selectedModel.modelId,
              snapshot?.runningModels ?? [],
            )}
            error={selectionError}
            executionTargets={siteTargets}
            memoryPending={panelMemoryPending}
            model={selectedModel}
            onBack={() => panelNavigation.back()}
            onDeploy={() =>
              startDeploy(
                selectedModel,
                selection.kind === "catalog-profile" ? selection.profileId : undefined,
                panelChips?.chips ?? catalogViewCodec.parse(location.search).predicates,
                panelScenario,
                panelBasis,
              )
            }
            onOpenChange={onInspectorOpenChange}
            onOpenDeployment={(modelDeploymentId) =>
              openDeployment({
                page: "models",
                resourceId: modelDeploymentId,
                screen: "deployment",
              })
            }
            onSelectProfile={(profileId) =>
              panelNavigation.navigatePanel({
                pathname: panelPagePath("model-catalog"),
                search: appendPanel(location.search, {
                  page: "model-catalog",
                  profileId,
                  resourceId: selectedModel.modelId,
                  screen: "catalog-profile",
                }),
              })
            }
            open={Boolean(urlSelection)}
            openerRef={openerRef}
            pending={pendingActions.has(catalogPendingKey(selectedModel.modelId))}
            profileId={selection.kind === "catalog-profile" ? selection.profileId : undefined}
            row={panelRow ?? modelRows.find((row) => row.modelId === selectedModel.modelId)!}
            screen={selection.kind === "catalog-profile" ? "profile-detail" : "overview"}
            suspendDismissal={Boolean(deployModel)}
          />
        ) : null}

        {deployModel ? (
          <ModelDeployDialog
            applications={applications}
            error={selectionError}
            existingRouteNames={routeViews.flatMap((view) => (view.route ? [view.route.name] : []))}
            initialProfileId={deployProfileId}
            catalogChips={deployChips}
            deployments={snapshot?.runningModels ?? noDeployments}
            model={deployModel}
            profileEstimates={
              sameSizingScenario(deployScenario, scenario) ? profileEstimates : undefined
            }
            memoryBasis={deployBasis}
            scenario={deployScenario}
            onOpenChange={(open) => {
              if (!open && !pendingActionsRef.current.has(catalogPendingKey(deployModel.modelId))) {
                setDeployModel(undefined);
                setDeployProfileId(undefined);
                setRouteConflict(undefined);
              }
            }}
            routeConflict={routeConflict}
            engineReleases={engineReleases ?? []}
            hardwareClasses={hardwareClasses ?? []}
            onEstimate={async ({ configuration, placement, profileId }) => {
              if (!client.model.estimateLocalModelResources)
                throw new Error("The model service does not support local resource estimates.");
              const response = await client.model.estimateLocalModelResources(
                create(EstimateLocalModelResourcesRequestSchema, {
                  modelId: deployModel.modelId,
                  profileId,
                  configuration: create(LocalModelDeploymentConfigurationSchema, configuration),
                  placement,
                }),
              );
              // Without a placement, a profile supporting several hardware classes gets one
              // estimate per class, and only a target tells which one applies.
              const [estimate, ...others] = response.hardwareClassEstimates;
              if (others.length) throw new Error("it depends on the selected target's hardware");
              return {
                launchRejection: estimate?.launchRejection,
                placementPreview: response.placementPreview,
              };
            }}
            onPreview={async (values) =>
              (await previewLocalDeployment(client, deployModel.modelId, values)).snapshot
            }
            onPreviewMemory={async (values) => {
              const { advisories, snapshot } = await previewLocalDeployment(
                client,
                deployModel.modelId,
                values,
              );
              const minimum =
                snapshot.hardwareSupport?.hardwareRequirements?.minimumMemoryBytesPerDevice;
              return {
                acceleratorMemoryArchitecture:
                  snapshot.selectedTarget?.acceleratorMemoryArchitecture ??
                  AcceleratorMemoryArchitecture.UNSPECIFIED,
                advisories,
                engineRelease: snapshot.engineArtifact && {
                  engineReleaseId: snapshot.engineArtifact.engineReleaseId,
                  version: snapshot.engineArtifact.engineVersion,
                },
                memoryPoolTotalBytes: snapshot.selectedTarget?.acceleratorMemoryPoolTotalBytes,
                minimumMemoryBytesPerDevice: minimum || undefined,
              };
            }}
            onSubmit={({ configuration, exposure, name, placement, profileId }) => {
              void load(
                deployModel,
                profileId,
                create(LocalModelDeploymentConfigurationSchema, configuration),
                placement,
                exposure,
                name,
              );
            }}
            listConfiguredExecutionTargets={listConfiguredExecutionTargets}
            listProfileCompatibility={listProfileCompatibility}
            openerRef={deployDialogOpenerRef}
            open
            pending={pendingActions.has(catalogPendingKey(deployModel.modelId))}
          />
        ) : null}
        {deployTemplate ? (
          <UpstreamDeployDialog
            open
            template={deployTemplate}
            error={upstreamError}
            existingRouteNames={routeViews.flatMap((view) => (view.route ? [view.route.name] : []))}
            onOpenChange={(open) => {
              if (!open && !upstreamDeployPendingRef.current) {
                deployTemplateRef.current = undefined;
                setDeployTemplate(undefined);
                setUpstreamError(undefined);
                setRouteConflict(undefined);
              }
            }}
            onSubmit={(values) => {
              void deployUpstream(deployTemplate, values);
            }}
            pending={upstreamDeployPending}
            routeConflict={routeConflict}
          />
        ) : null}
      </div>
    </div>
  );
}

type CatalogSelection =
  | { kind: "catalog"; modelId: string }
  | { kind: "catalog-profile"; modelId: string; profileId: string };

interface DeployError {
  message: string;
  selection: CatalogSelection;
}

const connectUpstreamLabel = "Connect upstream";

/**
 * Page action opening an UpstreamModelTemplate's deploy dialog. A single template opens directly;
 * several open a menu naming each one.
 */
function ConnectUpstreamModelAction({
  onSelect,
  templates,
}: {
  onSelect: (template: UpstreamModelTemplate) => void;
  templates: readonly UpstreamModelTemplate[];
}) {
  const label = (
    <>
      <Plus data-icon="inline-start" />
      {/* Phone widths keep the page heading and this action on one row, so only "Connect" shows. */}
      <span>
        Connect<span className="max-sm:hidden"> upstream</span>
      </span>
    </>
  );

  if (templates.length === 1) {
    return (
      <Button
        aria-label={connectUpstreamLabel}
        onClick={() => onSelect(templates[0])}
        size="console"
        type="button"
        variant="outline"
      >
        {label}
      </Button>
    );
  }

  return (
    <DropdownMenu>
      <DropdownMenuTrigger
        aria-label={connectUpstreamLabel}
        render={<Button size="console" type="button" variant="outline" />}
      >
        {label}
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="border border-border" sideOffset={6}>
        {templates.map((template) => (
          <DropdownMenuItem
            key={template.upstreamModelTemplateId}
            onClick={() => onSelect(template)}
          >
            {template.displayName || template.upstreamModelTemplateId}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

const noRouteViews: readonly InferenceRouteView[] = [];

const noDeployments: readonly ModelDeployment[] = [];

function catalogSelectionFromEntry(entry: PanelEntry | undefined): CatalogSelection | undefined {
  if (!entry || entry.page !== "model-catalog" || !entry.resourceId) return undefined;
  if (entry.screen === "catalog") return { kind: "catalog", modelId: entry.resourceId };
  if (entry.screen === "catalog-profile" && entry.profileId)
    return { kind: "catalog-profile", modelId: entry.resourceId, profileId: entry.profileId };
  return undefined;
}

function catalogSelectionEntry(selection: CatalogSelection): PanelEntry {
  if (selection.kind === "catalog")
    return { page: "model-catalog", resourceId: selection.modelId, screen: "catalog" };
  return {
    page: "model-catalog",
    profileId: selection.profileId,
    resourceId: selection.modelId,
    screen: "catalog-profile",
  };
}

function isSameSelection(
  left: CatalogSelection | undefined,
  right: CatalogSelection | undefined,
): boolean {
  if (!left || !right || left.kind !== right.kind) {
    return false;
  }

  if (left.kind === "catalog" && right.kind === "catalog") {
    return left.modelId === right.modelId;
  }

  return (
    left.kind === "catalog-profile" &&
    right.kind === "catalog-profile" &&
    left.modelId === right.modelId &&
    left.profileId === right.profileId
  );
}

function catalogPendingKey(modelId: string): string {
  return `catalog:${modelId}`;
}

/**
 * Resolves a local deployment without creating it, returning the resolved snapshot and the
 * non-blocking advisories the model service reports for it.
 */
async function previewLocalDeployment(
  client: ModelClient,
  modelId: string,
  {
    configuration,
    placement,
    profileId,
  }: Pick<CreateLocalModelDeploymentRequest, "placement" | "profileId"> & {
    configuration: Parameters<typeof create<typeof LocalModelDeploymentConfigurationSchema>>[1];
  },
): Promise<{
  advisories: readonly LocalModelDeploymentAdvisory[];
  snapshot: ResolvedLocalModelDeploymentSnapshot;
}> {
  if (!client.model.previewLocalModelDeployment)
    throw new Error(
      "Preview is unavailable because the model service does not support local deployment previews.",
    );
  const response = await client.model.previewLocalModelDeployment(
    create(CreateLocalModelDeploymentRequestSchema, {
      modelId,
      profileId,
      placement,
      configuration: create(LocalModelDeploymentConfigurationSchema, configuration),
    }),
  );
  if (!response.resolvedSnapshot)
    throw new Error("Preview response protocol error: missing resolved deployment snapshot.");
  return { advisories: response.advisories, snapshot: response.resolvedSnapshot };
}
